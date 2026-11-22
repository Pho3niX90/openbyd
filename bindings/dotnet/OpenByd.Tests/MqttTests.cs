using System.Collections.Concurrent;
using System.Text.Json;
using System.Text.Json.Nodes;
using OpenByd.Models;
using OpenByd.Testing;
using static OpenByd.Tests.Names;

namespace OpenByd.Tests;

/// <summary>Pushes over the TLS test broker, commands acknowledged by push, and the car layer.</summary>
[Collection(Sockets.Name)]
public sealed class MqttTests
{
    private static string Realtime(int lockState, long time, int percent = 80) => $$"""
        {
            "onlineState": 1, "time": {{time}}, "elecPercent": {{percent}},
            "leftFrontDoorLock": {{lockState}}, "rightFrontDoorLock": {{lockState}},
            "leftRearDoorLock": {{lockState}}, "rightRearDoorLock": {{lockState}}
        }
        """;

    private static string VehicleInfo(string data) =>
        $$"""{"event": "vehicleInfo", "vin": "{{Vin}}", "data": {"respondData": {{data}} } }""";

    private static string Acknowledged(string serial) =>
        $$"""{"event": "remoteControl", "vin": "{{Vin}}", "data": {"uuid": "{{serial}}", "respondData": {"res": 2} } }""";

    private static LockState?[] Doors(Car car)
    {
        var state = car.State.Realtime;
        return [state?.LeftFrontDoorLock, state?.RightFrontDoorLock, state?.LeftRearDoorLock, state?.RightRearDoorLock];
    }

    private static readonly LockState?[] AllLocked = [LockState.Locked, LockState.Locked, LockState.Locked, LockState.Locked];

    private static readonly LockState?[] AllUnlocked =
        [LockState.Unlocked, LockState.Unlocked, LockState.Unlocked, LockState.Unlocked];

    [Fact]
    public async Task PushesReachTheCallbacks()
    {
        using var world = new World();
        var infos = new ConcurrentQueue<(string Vin, VehicleRealtimeData Data)>();
        var events = new ConcurrentQueue<(string Event, string Vin, IReadOnlyDictionary<string, JsonElement> Data)>();
        var reported = new ConcurrentQueue<string>();
        var connects = 0;
        var client = world.Client(
            new ClientOptions
            {
                TestTls = true,
                OnVehicleInfo = (vin, data) =>
                {
                    infos.Enqueue((vin, data));
                    throw new InvalidOperationException("info bug");
                },
                OnMqttEvent = (name, vin, data) => events.Enqueue((name, vin, data)),
                OnMqttConnect = () => Interlocked.Increment(ref connects),
                OnCallbackError = (callback, _) => reported.Enqueue(callback),
            },
            world.Config(mqttEnabled: true));
        await client.LoginAsync();
        Assert.True(client.MqttIsRunning);
        await world.UntilSubscribed(client);
        await World.Until("OnMqttConnect", () => Volatile.Read(ref connects) > 0);
        world.Push(client, VehicleInfo("""{"elecPercent": 64, "time": 1700000000}"""));
        world.Push(client, $$"""{"event": "smartCharge", "vin": "{{Vin}}", "data": {"respondData": {"elecPercent": 64} } }""");
        await World.Until("two events", () => events.Count == 2);
        // The throwing OnVehicleInfo did not stop the next push.
        world.Push(client, VehicleInfo("""{"elecPercent": 65, "time": 1700000001}"""));
        await World.Until("two infos", () => infos.Count == 2);

        Assert.Equal(new (string, double?)[] { (Vin, 64), (Vin, 65) }, infos.Select(info => (info.Vin, info.Data.ElecPercent)));
        var first = events.First();
        Assert.Equal(("vehicleInfo", Vin, 64), (first.Event, first.Vin, first.Data["elecPercent"].GetInt32()));
        Assert.Equal("smartCharge", events.ElementAt(1).Event);
        await World.Until("two reports", () => reported.Count(callback => callback == "OnVehicleInfo") >= 2);
        await client.StartMqttAsync();
    }

    [Fact]
    public async Task ACommandIsAcknowledgedByPush()
    {
        using var world = new World();
        world.Server.Reply(Remote, Reply.Data(World.Json("""{"requestSerial": "C1"}""")));
        var acks = new ConcurrentQueue<CommandAckEvent>();
        var lifecycle = new ConcurrentQueue<CommandLifecycleStatus>();
        var client = world.Client(
            new ClientOptions
            {
                TestTls = true,
                OnCommandAck = acks.Enqueue,
                OnCommandLifecycle = change => lifecycle.Enqueue(change.Status),
            },
            world.Config(mqttEnabled: true));
        await client.LoginAsync();
        await world.UntilSubscribed(client);
        await client.VerifyCommandAccessAsync(Vin);
        var locking = client.LockAsync(Vin);
        await World.Until("the command", () => world.Server.Requests(Remote).Count == 1);
        await Task.Delay(50);
        world.Push(client, Acknowledged("C1"));
        var result = await locking;
        await World.Until("the ack callback", () => acks.Count == 1);
        await World.Until("the lifecycle callbacks", () => lifecycle.Count == 2);

        Assert.Equal((true, "C1"), (result.Success, result.RequestSerial));
        Assert.Empty(world.Server.Requests(RemoteResult));
        var ack = Assert.Single(acks);
        Assert.Equal(("C1", true), (ack.RequestSerial, ack.Success));
        Assert.Equal([CommandLifecycleStatus.Registered, CommandLifecycleStatus.Matched], lifecycle.ToArray());
        var diagnostics = client.GetCommandAckDiagnostics();
        Assert.Equal((1L, 0L), (diagnostics.Matched, diagnostics.Pending));
    }

    [Fact]
    public async Task ClosingEndsAWaitForAPushLikeATimeout()
    {
        // pyBYD #98: the read goes on to poll over HTTP instead of being cancelled.
        using var world = new World();
        world.Server.Reply(Gps, Reply.Data(World.Json("""{"requestSerial": "G1"}""")));
        world.Server.Reply(
            "/control/getGpsInfoResult",
            Reply.Data(World.Json("""{"requestSerial": "G1", "latitude": 52.0, "longitude": 4.0}""")));
        var client = world.Client(config: world.Config(mqttEnabled: true));
        await client.LoginAsync();
        await world.UntilSubscribed(client);
        var waiting = client.GetGpsInfoAsync(Vin, mqttTimeout: 30, pollAttempts: 1, pollInterval: 0.0);
        await World.Until("the trigger", () => world.Server.Requests(Gps).Count == 1);
        await Task.Delay(50);
        client.Close();
        Assert.Equal(52.0, (await waiting).Latitude);
        Assert.Single(world.Server.Requests("/control/getGpsInfoResult"));
    }

    [Fact]
    public async Task ACarMergesProjectsAndRollsBack()
    {
        using var world = new World();
        world.Server.Reply(Names.Realtime, Reply.Data(World.Json(Realtime(1, 1_700_000_000))));
        world.Server.Reply(Gps, Reply.Data(World.Json("""{"latitude": 52.0, "longitude": 4.0}""")));
        world.Server.Reply(Homepage, Reply.Data(World.Json("""{"elecPercent": 80, "chargingState": 0}""")));
        world.Server.Reply(
            "/vehicleInfo/vehicle/getEnergyConsumption", Reply.Data(World.Json("""{"totalEnergy": "15.2kW·h/100km"}""")));
        world.Server.Reply("/control/smartCharge/getAcCurrentLimit", Reply.Data(World.Json("""{"acCurrentLimit": 16}""")));
        var changes = new ConcurrentQueue<string>();
        var client = world.Client(config: world.Config(mqttEnabled: true));
        await client.LoginAsync();
        await world.UntilSubscribed(client);
        await client.VerifyCommandAccessAsync(Vin);

        var car = await client.GetCarAsync(Vin, onStateChanged: (vin, _) => changes.Enqueue(vin));
        Assert.Same(car, await client.GetCarAsync(Vin));
        Assert.Equal(Vin, car.Vin);
        Assert.True(car.Capabilities.Lock);
        Assert.True(car.Lock.Available);
        Assert.True(car.Hvac.Available);
        Assert.Equal([15.0, 31.0], car.Hvac.TempRange);
        Assert.Equal(14, car.Hvac.TempOffset);
        _ = car.Seat.IsAvailable(SeatPosition.Driver, SeatMode.Heat);
        Assert.Throws<DataException>(() => car.Seat.IsAvailable((SeatPosition)99, SeatMode.Heat));
        var features = car.Features;
        Assert.True(features.Lock.Available);
        Assert.NotNull(features.Seat.RearRight);

        // Every section.
        Assert.Equal(80, (await car.UpdateRealtimeAsync()).ElecPercent);
        await car.UpdateHvacAsync();
        Assert.Equal(52, (await car.UpdateGpsAsync()).Latitude);
        await car.UpdateChargingAsync();
        await car.UpdateEnergyAsync();
        Assert.NotNull(await car.GetAcCurrentLimitAsync());
        var state = car.State;
        Assert.Equal(Vin, state.Vehicle.Vin);
        Assert.NotNull(state.Realtime);
        Assert.NotNull(state.Hvac);
        Assert.NotNull(state.Gps);
        Assert.NotNull(state.Charging);
        Assert.NotNull(state.Energy);
        Assert.NotNull(state.ChargingSchedule);
        await World.Until("state change callbacks", () => changes.Count >= 6);
        Assert.All(changes, vin => Assert.Equal(Vin, vin));
        Assert.Equal(AllUnlocked, Doors(car));

        // A command acknowledged by push: projected at once, then confirmed.
        world.Server.Reply(Remote, Reply.Data(World.Json("""{"requestSerial": "C1"}""")));
        var locking = car.Lock.LockAsync();
        await World.Until("the command", () => world.Server.Requests(Remote).Count == 1);
        Assert.Equal(AllLocked, Doors(car));
        await Task.Delay(50);
        world.Push(client, Acknowledged("C1"));
        await locking;
        world.Push(client, VehicleInfo(Realtime(2, 1_700_000_200)));
        await World.Until("confirmation", () => Doors(car).SequenceEqual(AllLocked));

        // A refused command: rolled back.
        world.Server.Script(Remote, Reply.Code("5005"));
        await Assert.ThrowsAsync<BydControlPasswordException>(() => car.Lock.UnlockAsync());
        Assert.Equal(AllLocked, Doors(car));

        // Pushes handed to the car directly, and their validation.
        car.HandleMqttCharging(World.Raw("""{"elecPercent": 81, "chargingState": 1}"""));
        car.HandleMqttEnergy(World.Raw("""{"time": 1700000300}"""));
        car.HandleMqttRealtime(World.Raw(Realtime(2, 1_700_000_400, 79)));
        await World.Until("the charging push", () => car.State.Charging?.Soc == 81);
        await World.Until("the realtime push", () => car.State.Realtime?.ElecPercent == 79);
        car.HandleMqttStatusPush(
            World.Raw("""{"elecPercent": 77}"""),
            hvac: World.Raw("""{"acSwitch": 1, "tempInCar": 22}"""),
            charging: World.Raw("""{"elecPercent": 77, "chargingState": 1}"""));
        await World.Until(
            "the status push",
            () => car.State.Realtime?.ElecPercent == 77
                && car.State.Hvac?.TempInCar == 22
                && car.State.Charging?.Soc == 77);
        Assert.Throws<ValidationException>(() => car.HandleMqttCharging(World.Raw("""{"elecPercent": "lots"}""")));
        Assert.Throws<ValidationException>(() => car.HandleMqttStatusPush(
            World.Raw("""{"elecPercent": 55}"""),
            hvac: World.Raw("""{"tempInCar": "warm"}""")));

        // The client closed: the car's calls fail as a closed client's do.
        client.Close();
        await Assert.ThrowsAnyAsync<BydException>(() => car.UpdateHvacAsync());
        car.Close();
    }

    [Fact]
    public async Task EveryCarCommandReachesTheClient()
    {
        using var world = new World();
        world.Server.Reply(Remote, Reply.Data(World.Json("""{"controlState": 1, "requestSerial": "C1"}""")));
        world.Server.Reply(
            "/control/remoteControlBatteryHeat", Reply.Data(World.Json("""{"controlState": 1, "requestSerial": "C2"}""")));
        world.Server.Reply(
            "/control/smartCharge/changeChargeStatue", Reply.Data(World.Json("""{"res": 2, "requestSerial": "S"}""")));
        world.Server.Reply("/control/smartCharge/saveOrUpdate", Reply.Data(World.Json("""{"res": 2, "requestSerial": "S"}""")));
        world.Server.Reply(Homepage, Reply.Data(World.Json("""{"elecPercent": 80}""")));
        world.Server.Reply(LatestConfig, Reply.Data(new JsonObject
        {
            [Vin] = World.Functions(
                "1001", "1005", "1006", "1007", "1008", "1009", "1012", "1020", "1021", "1026",
                "10300002", "10030010", "10030002", "10030001", "10030004"),
        }));
        var client = world.Client();
        await client.VerifyCommandAccessAsync(Vin);
        var car = await client.GetCarAsync(
            Vin,
            vehicle: World.Raw($$"""{"vin": "{{Vin}}", "energyType": "0", "vehicleFunLearnInfo": {"openWindowLearnInfo": 1} }"""),
            projectionTtl: 10);
        var schedule = new ClimateScheduleParams { BookingId = 1, BookingTime = 1_700_000_000 };
        var commands = new (string Name, Func<Task> Run)[]
        {
            ("lock", () => car.Lock.LockAsync()),
            ("unlock", () => car.Lock.UnlockAsync()),
            ("hvac.start", () => car.Hvac.StartAsync(temperature: 22, duration: 10)),
            ("hvac.start with defaults", () => car.Hvac.StartAsync()),
            ("hvac.stop", () => car.Hvac.StopAsync()),
            ("hvac.schedule", () => car.Hvac.ScheduleAsync(schedule)),
            ("seat.heat", () => car.Seat.HeatAsync(SeatPosition.Driver, SeatLevel.High)),
            ("seat.ventilation", () => car.Seat.VentilationAsync(SeatPosition.Copilot, SeatLevel.Low)),
            ("steering", () => car.Steering.HeatAsync(true)),
            ("battery", () => car.Battery.HeatAsync(true)),
            ("find", () => car.Finder.FindAsync()),
            ("flash", () => car.Finder.FlashLightsAsync()),
            ("windows.close", () => car.Windows.CloseAsync()),
            ("windows.open", () => car.Windows.OpenAsync()),
            ("trunk.open", () => car.Trunk.OpenAsync()),
            ("trunk.close", () => car.Trunk.CloseAsync()),
            ("StartChargingAsync", () => car.StartChargingAsync()),
            ("SaveChargingScheduleAsync", () => car.SaveChargingScheduleAsync("22:00", "06:00", "e", enabled: true)),
        };
        var unsupported = new List<string>();
        foreach (var (name, run) in commands)
        {
            try
            {
                await run();
            }
            catch (BydEndpointNotSupportedException)
            {
                unsupported.Add(name);
            }
        }
        // Every feature the car has goes through.
        Assert.Empty(unsupported);
        Assert.True(world.Server.Requests(Remote).Count >= 12);
        await Assert.ThrowsAsync<BydEndpointNotSupportedException>(() => car.StopChargingAsync());
        await Assert.ThrowsAsync<DataException>(() => car.Seat.HeatAsync((SeatPosition)99, SeatLevel.Low));
        await Assert.ThrowsAsync<DataException>(() => car.Seat.HeatAsync(SeatPosition.Driver, (SeatLevel)7));
        Assert.True(car.Windows.OpenAvailable);
        Assert.True(car.Finder.FindAvailable);
        Assert.True(car.Trunk.CloseAvailable);

        // The client's own commands, with their parameter objects.
        var started = await client.StartClimateAsync(Vin, new ClimateStartParams { Temperature = 21, TimeSpan = 2 });
        Assert.True(started.Success);
        var sent = world.Server.Requests(Remote)[^1].Inner.GetProperty("controlParamsMap").GetString();
        Assert.Contains("\"timeSpan\":2", sent);
        // This car's seats are unknown to the gate; that the parameters arrive is the point.
        var seats = await Assert.ThrowsAsync<BydEndpointNotSupportedException>(
            () => client.SetSeatClimateAsync(Vin, new SeatClimateParams { MainHeat = 2 }));
        Assert.Contains("seat_target", seats.Message);
        await Assert.ThrowsAsync<ValidationException>(
            () => client.SetSeatClimateAsync(Vin, new SeatClimateParams { MainHeat = 9 }));
        Assert.True((await client.SetBatteryHeatAsync(Vin, new BatteryHeatParams { On = true })).Success);
        Assert.True((await client.ScheduleClimateAsync(Vin, schedule)).Success);
        foreach (var run in new Func<Task<RemoteControlResult>>[]
        {
            () => client.StopClimateAsync(Vin),
            () => client.FlashLightsAsync(Vin),
            () => client.CloseWindowsAsync(Vin),
            () => client.OpenWindowsAsync(Vin),
            () => client.FindCarAsync(Vin),
            () => client.OpenTrunkAsync(Vin),
            () => client.CloseTrunkAsync(Vin),
        })
        {
            Assert.True((await run()).Success);
        }
        Assert.Equal(2, (await client.StartChargingAsync(Vin, pollAttempts: 1)).Res);
        Assert.Equal(
            2, (await client.SaveChargingScheduleAsync(Vin, "22:00", "06:00", "e", enabled: false, pollAttempts: 1)).Res);
        await Assert.ThrowsAsync<BydEndpointNotSupportedException>(() => client.StopChargingAsync(Vin));
    }
}
