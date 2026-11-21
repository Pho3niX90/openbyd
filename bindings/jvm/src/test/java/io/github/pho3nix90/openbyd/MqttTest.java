package io.github.pho3nix90.openbyd;

import static io.github.pho3nix90.openbyd.World.GPS;
import static io.github.pho3nix90.openbyd.World.HOMEPAGE;
import static io.github.pho3nix90.openbyd.World.LATEST_CONFIG;
import static io.github.pho3nix90.openbyd.World.REALTIME;
import static io.github.pho3nix90.openbyd.World.REMOTE;
import static io.github.pho3nix90.openbyd.World.REMOTE_RESULT;
import static io.github.pho3nix90.openbyd.World.VIN;
import static io.github.pho3nix90.openbyd.World.await;
import static io.github.pho3nix90.openbyd.World.functions;
import static io.github.pho3nix90.openbyd.World.sleep;
import static io.github.pho3nix90.openbyd.World.thrown;
import static io.github.pho3nix90.openbyd.World.until;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.pho3nix90.openbyd.model.BatteryHeatParams;
import io.github.pho3nix90.openbyd.model.CarFeatures;
import io.github.pho3nix90.openbyd.model.CarScheduleOptions;
import io.github.pho3nix90.openbyd.model.ChargePollOptions;
import io.github.pho3nix90.openbyd.model.ChargingStatus;
import io.github.pho3nix90.openbyd.model.ClimateOptions;
import io.github.pho3nix90.openbyd.model.ClimateScheduleParams;
import io.github.pho3nix90.openbyd.model.ClimateStartParams;
import io.github.pho3nix90.openbyd.model.CommandAckDiagnostics;
import io.github.pho3nix90.openbyd.model.CommandAckEvent;
import io.github.pho3nix90.openbyd.model.CommandLifecycleStatus;
import io.github.pho3nix90.openbyd.model.GpsInfo;
import io.github.pho3nix90.openbyd.model.LockState;
import io.github.pho3nix90.openbyd.model.PollOptions;
import io.github.pho3nix90.openbyd.model.RemoteControlResult;
import io.github.pho3nix90.openbyd.model.SaveScheduleOptions;
import io.github.pho3nix90.openbyd.model.SeatClimateParams;
import io.github.pho3nix90.openbyd.model.SeatLevel;
import io.github.pho3nix90.openbyd.model.SeatMode;
import io.github.pho3nix90.openbyd.model.SeatPosition;
import io.github.pho3nix90.openbyd.model.VehicleRealtimeData;
import io.github.pho3nix90.openbyd.model.VehicleSnapshot;
import io.github.pho3nix90.openbyd.testing.RecordedRequest;
import io.github.pho3nix90.openbyd.testing.Reply;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicInteger;
import org.jspecify.annotations.Nullable;
import org.junit.jupiter.api.Test;

/** Pushes over the TLS test broker, commands acknowledged by push, and the car layer. */
class MqttTest {
    private static Map<String, Object> realtime(int lock, long time, int percent) {
        Map<String, Object> data = new LinkedHashMap<>();
        data.put("onlineState", 1);
        data.put("time", time);
        data.put("elecPercent", percent);
        data.put("leftFrontDoorLock", lock);
        data.put("rightFrontDoorLock", lock);
        data.put("leftRearDoorLock", lock);
        data.put("rightRearDoorLock", lock);
        return data;
    }

    private static Map<String, Object> vehicleInfo(Map<String, Object> data) {
        return Map.of("event", "vehicleInfo", "vin", VIN, "data", Map.of("respondData", data));
    }

    private static Map<String, Object> acknowledged(String serial) {
        Map<String, Object> data = Map.of("uuid", serial, "respondData", Map.of("res", 2));
        return Map.of("event", "remoteControl", "vin", VIN, "data", data);
    }

    private static List<LockState> doors(Car car) {
        VehicleRealtimeData state = car.state().realtime();
        if (state == null) {
            return List.of();
        }
        return Arrays.asList(
                state.leftFrontDoorLock(),
                state.rightFrontDoorLock(),
                state.leftRearDoorLock(),
                state.rightRearDoorLock());
    }

    private static @Nullable Long soc(@Nullable ChargingStatus charging) {
        return charging == null ? null : charging.soc();
    }

    private static @Nullable Double percent(@Nullable VehicleRealtimeData realtime) {
        return realtime == null ? null : realtime.elecPercent();
    }

    private static List<LockState> all(LockState state) {
        return List.of(state, state, state, state);
    }

    @Test
    void pushesReachTheCallbacks() throws Exception {
        try (World world = new World()) {
            List<Map.Entry<String, VehicleRealtimeData>> infos = new CopyOnWriteArrayList<>();
            List<List<Object>> events = new CopyOnWriteArrayList<>();
            List<String> reported = new CopyOnWriteArrayList<>();
            List<String> threads = new CopyOnWriteArrayList<>();
            AtomicInteger connects = new AtomicInteger();
            ClientOptions options =
                    new ClientOptions()
                            .onVehicleInfo(
                                    (vin, data) -> {
                                        infos.add(Map.entry(vin, data));
                                        throw new IllegalStateException("info bug");
                                    })
                            .onMqttEvent(
                                    (event, vin, data) -> {
                                        threads.add(Thread.currentThread().getName());
                                        events.add(List.of(event, vin, data));
                                    })
                            .onMqttConnect(connects::incrementAndGet)
                            .onCallbackError((callback, error) -> reported.add(callback));
            Client client = world.client(world.config().mqttEnabled(true), options);
            await(client.login());
            assertTrue(client.mqttIsRunning());
            world.untilSubscribed(client);
            until("onMqttConnect", () -> connects.get() > 0);
            world.push(client, vehicleInfo(Map.of("elecPercent", 64, "time", 1_700_000_000)));
            Map<String, Object> charge = Map.of("respondData", Map.of("elecPercent", 64));
            world.push(client, Map.of("event", "smartCharge", "vin", VIN, "data", charge));
            until("two events", () -> events.size() == 2);
            // The throwing onVehicleInfo did not stop the next push.
            world.push(client, vehicleInfo(Map.of("elecPercent", 65, "time", 1_700_000_001)));
            until("two infos", () -> infos.size() == 2);

            assertEquals(List.of(VIN, VIN), infos.stream().map(Map.Entry::getKey).toList());
            assertEquals(List.of(64.0, 65.0), infos.stream().map(info -> info.getValue().elecPercent()).toList());
            assertEquals(List.of("vehicleInfo", VIN), events.get(0).subList(0, 2));
            assertEquals(64, assertInstanceOf(Map.class, events.get(0).get(2)).get("elecPercent"));
            assertEquals("smartCharge", events.get(1).get(0));
            until("two reports", () -> reported.stream().filter("onVehicleInfo"::equals).count() >= 2);
            // Callbacks run on the client's own thread, never on one of the core's.
            assertTrue(threads.stream().allMatch(name -> name.startsWith("openbyd-callbacks-")), threads.toString());
            await(client.startMqtt());
        }
    }

    @Test
    void aCommandIsAcknowledgedByPush() throws Exception {
        try (World world = new World()) {
            world.server.reply(REMOTE, Reply.data(Map.of("requestSerial", "C1")));
            List<CommandAckEvent> acks = new CopyOnWriteArrayList<>();
            List<CommandLifecycleStatus> lifecycle = new CopyOnWriteArrayList<>();
            ClientOptions options =
                    new ClientOptions().onCommandAck(acks::add).onCommandLifecycle(event -> lifecycle.add(event.status()));
            Client client = world.client(world.config().mqttEnabled(true), options);
            await(client.login());
            world.untilSubscribed(client);
            await(client.verifyCommandAccess(VIN));
            CompletableFuture<RemoteControlResult> locking = client.lock(VIN);
            until("the command", () -> world.server.requests(REMOTE).size() == 1);
            sleep(50);
            world.push(client, acknowledged("C1"));
            RemoteControlResult result = await(locking);
            until("the ack callback", () -> acks.size() == 1);

            assertTrue(result.success());
            assertEquals("C1", result.requestSerial());
            assertEquals(List.of(), world.server.requests(REMOTE_RESULT), "acknowledged by push, not polled");
            assertEquals("C1", acks.get(0).requestSerial());
            assertTrue(acks.get(0).success());
            until("the lifecycle", () -> lifecycle.size() == 2);
            assertEquals(List.of(CommandLifecycleStatus.REGISTERED, CommandLifecycleStatus.MATCHED), lifecycle);
            CommandAckDiagnostics diagnostics = client.getCommandAckDiagnostics();
            assertEquals(List.of(1L, 0L), List.of(diagnostics.matched(), diagnostics.pending()));
        }
    }

    @Test
    void closingCancelsAWaitForAPush() throws Exception {
        try (World world = new World()) {
            world.server.reply(GPS, Reply.data(Map.of("requestSerial", "G1")));
            Client client = world.client(world.config().mqttEnabled(true));
            await(client.login());
            world.untilSubscribed(client);
            CompletableFuture<GpsInfo> waiting = client.getGpsInfo(VIN, new PollOptions().mqttTimeout(30));
            until("the trigger", () -> world.server.requests(GPS).size() == 1);
            sleep(50);
            client.close();
            assertInstanceOf(CancelledException.class, thrown(() -> waiting));
        }
    }

    @Test
    void aCarMergesProjectsAndRollsBack() throws Exception {
        try (World world = new World()) {
            world.server.reply(REALTIME, Reply.data(realtime(1, 1_700_000_000, 80)));
            world.server.reply(GPS, Reply.data(Map.of("latitude", 52.0, "longitude", 4.0)));
            world.server.reply(HOMEPAGE, Reply.data(Map.of("elecPercent", 80, "chargingState", 0)));
            world.server.reply(
                    "/vehicleInfo/vehicle/getEnergyConsumption", Reply.data(Map.of("totalEnergy", "15.2kW·h/100km")));
            world.server.reply("/control/smartCharge/getAcCurrentLimit", Reply.data(Map.of("acCurrentLimit", 16)));
            List<String> changes = new CopyOnWriteArrayList<>();
            Client client = world.client(world.config().mqttEnabled(true));
            await(client.login());
            world.untilSubscribed(client);
            await(client.verifyCommandAccess(VIN));

            Car car = await(client.getCar(VIN, new GetCarOptions().onStateChanged((vin, state) -> changes.add(vin))));
            assertSame(car, await(client.getCar(VIN)), "kept");
            assertEquals(VIN, car.vin());
            assertEquals(true, car.capabilities().lock());
            assertTrue(car.lock().available());
            assertTrue(car.hvac().available());
            assertEquals(List.of(15.0, 31.0), car.hvac().tempRange());
            assertEquals(14.0, car.hvac().tempOffset());
            car.seat().isAvailable(SeatPosition.DRIVER, SeatMode.HEAT);
            car.seat().isAvailable(SeatPosition.REAR_RIGHT, SeatMode.VENTILATION);
            CarFeatures features = car.features();
            assertEquals(features.seat().driver().heat(), car.seat().isAvailable(SeatPosition.DRIVER, SeatMode.HEAT));
            assertEquals(features.lock().available(), car.lock().available());

            // Every section.
            assertEquals(80.0, await(car.updateRealtime()).elecPercent());
            await(car.updateHvac());
            assertEquals(52.0, await(car.updateGps()).latitude());
            await(car.updateCharging());
            await(car.updateEnergy());
            assertNotNull(await(car.getAcCurrentLimit()));
            VehicleSnapshot state = car.state();
            assertEquals(VIN, state.vehicle().vin());
            assertNotNull(state.realtime(), "realtime");
            assertNotNull(state.hvac(), "hvac");
            assertNotNull(state.gps(), "gps");
            assertNotNull(state.charging(), "charging");
            assertNotNull(state.energy(), "energy");
            assertNotNull(state.chargingSchedule(), "charging_schedule");
            until("state change callbacks", () -> changes.size() >= 6);
            assertEquals(all(LockState.UNLOCKED), doors(car));

            // A command acknowledged by push: projected at once, then confirmed.
            world.server.reply(REMOTE, Reply.data(Map.of("requestSerial", "C1")));
            CompletableFuture<Void> locking = car.lock().lock();
            until("the command", () -> world.server.requests(REMOTE).size() == 1);
            assertEquals(all(LockState.LOCKED), doors(car), "projected while in flight");
            sleep(50);
            world.push(client, acknowledged("C1"));
            await(locking);
            world.push(client, vehicleInfo(realtime(2, 1_700_000_200, 80)));
            until("confirmation", () -> doors(car).equals(all(LockState.LOCKED)));

            // A refused command: rolled back.
            world.server.script(REMOTE, Reply.code("5005"));
            assertInstanceOf(BydControlPasswordException.class, thrown(() -> car.lock().unlock()));
            assertEquals(all(LockState.LOCKED), doors(car), "rolled back");

            // Pushes handed to the car directly, and their validation.
            car.handleMqttCharging(Map.of("elecPercent", 81, "chargingState", 1));
            car.handleMqttEnergy(Map.of("time", 1_700_000_300));
            car.handleMqttRealtime(realtime(2, 1_700_000_400, 79));
            until("the charging push", () -> Long.valueOf(81).equals(soc(car.state().charging())));
            until("the realtime push", () -> Double.valueOf(79).equals(percent(car.state().realtime())));
            assertThrows(ValidationException.class, () -> car.handleMqttCharging(Map.of("elecPercent", "lots")));
            assertThrows(DataException.class, () -> car.handleMqttEnergy(Map.of("time", Double.NaN)));

            // The client closed: the car's calls fail as a closed client's do.
            client.close();
            assertInstanceOf(BydException.class, thrown(car::updateHvac));
            assertThrows(BydException.class, car::state);
            car.close();
        }
    }

    @Test
    void everyCarCommandReachesTheClient() throws Exception {
        try (World world = new World()) {
            world.server.reply(REMOTE, Reply.data(Map.of("controlState", 1, "requestSerial", "C1")));
            world.server.reply(
                    "/control/remoteControlBatteryHeat", Reply.data(Map.of("controlState", 1, "requestSerial", "C2")));
            world.server.reply(
                    "/control/smartCharge/changeChargeStatue", Reply.data(Map.of("res", 2, "requestSerial", "S")));
            world.server.reply("/control/smartCharge/saveOrUpdate", Reply.data(Map.of("res", 2, "requestSerial", "S")));
            world.server.reply(HOMEPAGE, Reply.data(Map.of("elecPercent", 80)));
            Map<String, Object> everything =
                    functions(
                            "1001", "1005", "1006", "1007", "1008", "1009", "1012", "1020", "1021", "1026",
                            "10300002", "10030010", "10030002", "10030001", "10030004");
            world.server.reply(LATEST_CONFIG, Reply.data(Map.of(VIN, everything)));
            Client client = world.client();
            await(client.verifyCommandAccess(VIN));
            Map<String, Object> vehicle =
                    Map.of("vin", VIN, "energyType", "0", "vehicleFunLearnInfo", Map.of("openWindowLearnInfo", 1));
            Car car = await(client.getCar(VIN, new GetCarOptions().vehicle(vehicle).projectionTtl(10)));
            ClimateScheduleParams booking = new ClimateScheduleParams().bookingId(1).bookingTime(1_700_000_000);

            Map<String, Callable<CompletableFuture<?>>> commands = new LinkedHashMap<>();
            commands.put("lock", () -> car.lock().lock());
            commands.put("unlock", () -> car.lock().unlock());
            commands.put("hvac.start", () -> car.hvac().start(new ClimateOptions().temperature(22).duration(10)));
            commands.put("hvac.start with defaults", () -> car.hvac().start());
            commands.put("hvac.stop", () -> car.hvac().stop());
            commands.put("hvac.schedule", () -> car.hvac().schedule(booking));
            commands.put("seat.heat", () -> car.seat().heat(SeatPosition.DRIVER, SeatLevel.HIGH));
            commands.put("seat.ventilation", () -> car.seat().ventilation(SeatPosition.COPILOT, SeatLevel.LOW));
            commands.put("steering", () -> car.steering().heat(true));
            commands.put("battery", () -> car.battery().heat(true));
            commands.put("find", () -> car.finder().find());
            commands.put("flash", () -> car.finder().flashLights());
            commands.put("windows.close", () -> car.windows().close());
            commands.put("windows.open", () -> car.windows().open());
            commands.put("trunk.open", () -> car.trunk().open());
            commands.put("trunk.close", () -> car.trunk().close());
            commands.put("startCharging", car::startCharging);
            commands.put(
                    "saveChargingSchedule",
                    () -> car.saveChargingSchedule("22:00", "06:00", "e", new CarScheduleOptions().enabled(true)));
            List<String> unsupported = new ArrayList<>();
            for (Map.Entry<String, Callable<CompletableFuture<?>>> command : commands.entrySet()) {
                try {
                    await(command.getValue().call());
                } catch (BydEndpointNotSupportedException refused) {
                    unsupported.add(command.getKey());
                }
            }
            assertEquals(List.of(), unsupported, "every feature the car has goes through");
            assertTrue(world.server.requests(REMOTE).size() >= 12);
            assertInstanceOf(BydEndpointNotSupportedException.class, thrown(car::stopCharging));
            assertTrue(car.windows().openAvailable());
            assertTrue(car.windows().closeAvailable());
            assertTrue(car.finder().findAvailable());
            assertTrue(car.finder().flashAvailable());
            assertTrue(car.trunk().closeAvailable());
            assertTrue(car.trunk().openAvailable());
            assertTrue(car.steering().available());
            assertTrue(car.battery().available());

            // The client's own commands, with their parameter objects.
            ClimateStartParams climate = new ClimateStartParams().temperature(21).timeSpan(2);
            assertTrue(await(client.startClimate(VIN, climate)).success());
            List<RecordedRequest> sent = world.server.requests(REMOTE);
            Object parameters = assertInstanceOf(Map.class, sent.get(sent.size() - 1).inner()).get("controlParamsMap");
            assertTrue(String.valueOf(parameters).contains("\"timeSpan\":2"), String.valueOf(parameters));
            // This car's seats are unknown to the gate; that the parameters arrive is the point.
            Throwable seats = thrown(() -> client.setSeatClimate(VIN, new SeatClimateParams().mainHeat(2)));
            assertInstanceOf(BydEndpointNotSupportedException.class, seats);
            assertTrue(seats.getMessage().contains("seat_target"), seats.getMessage());
            assertInstanceOf(
                    ValidationException.class,
                    thrown(() -> client.setSeatClimate(VIN, new SeatClimateParams().mainHeat(9))));
            assertTrue(await(client.setBatteryHeat(VIN, new BatteryHeatParams(true))).success());
            assertTrue(await(client.scheduleClimate(VIN, booking)).success());
            List<Callable<CompletableFuture<RemoteControlResult>>> plain =
                    List.of(
                            () -> client.stopClimate(VIN),
                            () -> client.flashLights(VIN),
                            () -> client.closeWindows(VIN),
                            () -> client.openWindows(VIN),
                            () -> client.findCar(VIN),
                            () -> client.openTrunk(VIN),
                            () -> client.closeTrunk(VIN));
            for (Callable<CompletableFuture<RemoteControlResult>> run : plain) {
                assertTrue(await(run.call()).success());
            }
            assertEquals(2L, await(client.startCharging(VIN, new ChargePollOptions().pollAttempts(1))).res());
            SaveScheduleOptions saving = new SaveScheduleOptions().enabled(false).pollAttempts(1);
            assertEquals(2L, await(client.saveChargingSchedule(VIN, "22:00", "06:00", "e", saving)).res());
            assertInstanceOf(BydEndpointNotSupportedException.class, thrown(() -> client.stopCharging(VIN)));
        }
    }
}
