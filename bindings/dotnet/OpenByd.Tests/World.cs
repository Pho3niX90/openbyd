using System.Text.Json;
using System.Text.Json.Nodes;
using OpenByd.Models;
using OpenByd.Testing;

namespace OpenByd.Tests;

/// <summary>The synthetic account and car of the tests, and the endpoints they use.</summary>
internal static class Names
{
    public const string Vin = "TESTVIN0000000001";
    public const string User = "user@example.com";
    public const string Password = "secret";
    public const string Pin = "123456";

    public const string BrokerLookup = "/app/emqAuth/getEmqBrokerIp";
    public const string Vehicles = "/app/account/getAllListByUserId";
    public const string LatestConfig = "/vehicle/vehicleswitch/getLatestConfig";
    public const string Verify = "/vehicle/vehicleswitch/verifyControlPassword";
    public const string Hvac = "/control/getStatusNow";
    public const string Login = "/app/account/login";
    public const string Remote = "/control/remoteControl";
    public const string RemoteResult = "/control/remoteControlResult";
    public const string Realtime = "/vehicleInfo/vehicle/vehicleRealTimeRequest";
    public const string Homepage = "/control/smartCharge/homePage";
    public const string Gps = "/control/getGpsInfo";

    public static readonly string[] Functions =
        ["1001", "1005", "1006", "1012", "1020", "1021", "1026", "1027", "1030", "1033"];
}

/// <summary>
/// The tests that wait on sockets and timers. They run one after another, so that none is
/// slowed past its waits by the others; the tests that only compute run beside them.
/// </summary>
[CollectionDefinition(Name)]
public sealed class Sockets
{
    public const string Name = "sockets";
}

/// <summary>A fake BYD server and a test broker wired together, and clients pointed at both.</summary>
internal sealed class World : IDisposable
{
    private readonly List<Client> _clients = [];

    public World()
    {
        Server.Reply(Names.BrokerLookup, Reply.Data(new JsonObject { ["emqBorker"] = Broker.Address }));
        Server.Reply(Names.Vehicles, Reply.Data(Json("""[{"vin": "TESTVIN0000000001", "energyType": "0", "modelName": "SEAL"}]""")));
        Server.Reply(Names.LatestConfig, Reply.Data(new JsonObject { [Names.Vin] = Functions(Names.Functions) }));
        Server.Reply(Names.Verify, Reply.Data(Json("""{"ok": true}""")));
        Server.Reply(Names.Hvac, Reply.Data(Json("""{"acSwitch": 1, "tempInCar": 21}""")));
    }

    public FakeServer Server { get; } = new(new Account { Username = Names.User, Password = Names.Password });

    public TestBroker Broker { get; } = new();

    public static JsonNode Json(string text) => JsonNode.Parse(text)!;

    /// <summary>A JSON object as the methods that take a raw payload want it.</summary>
    public static IReadOnlyDictionary<string, JsonElement> Raw(string text)
    {
        using var document = JsonDocument.Parse(text);
        return document.RootElement.EnumerateObject().ToDictionary(field => field.Name, field => field.Value.Clone());
    }

    /// <summary>A latest config that lists these function numbers.</summary>
    public static JsonNode Functions(params string[] numbers) => new JsonObject
    {
        ["cfFixedList"] = new JsonArray(numbers.Select(number => (JsonNode)new JsonObject { ["functionNo"] = number }).ToArray()),
    };

    public BydConfig Config(
        bool mqttEnabled = false,
        string password = Names.Password,
        string? countryCode = null,
        string? language = null,
        DeviceProfile? device = null) => new()
        {
            Username = Names.User,
            Password = password,
            BaseUrl = Server.Url,
            MqttEnabled = mqttEnabled,
            MqttTimeout = 5,
            ControlPin = Names.Pin,
            CountryCode = countryCode,
            Language = language,
            Device = device,
        };

    /// <summary>A client that is disposed with the world. It trusts the test broker.</summary>
    public Client Client(ClientOptions? options = null, BydConfig? config = null)
    {
        var client = new Client(config ?? Config(), options ?? new ClientOptions { TestTls = true });
        lock (_clients)
        {
            _clients.Add(client);
        }
        return client;
    }

    public static SessionTokens Session(Client client) =>
        client.ExportSession() ?? throw new InvalidOperationException("the client has no session");

    public void Push(Client client, string body)
    {
        var reached = Broker.Push(Session(client), Json(body));
        Assert.Equal(1, reached);
    }

    public Task UntilSubscribed(Client client)
    {
        var topic = $"oversea/res/{Session(client).UserId}";
        return Until("a subscription", () => Broker.Subscribers(topic) > 0);
    }

    /// <summary>Waits until <paramref name="condition"/> is true.</summary>
    public static async Task Until(string what, Func<bool> condition, int timeoutMs = 10_000)
    {
        var deadline = Environment.TickCount64 + timeoutMs;
        while (!condition())
        {
            if (Environment.TickCount64 > deadline)
            {
                throw new TimeoutException($"timed out waiting for {what}");
            }
            await Task.Delay(5);
        }
    }

    /// <summary>The exception <paramref name="run"/> fails with; fails when it does not.</summary>
    public static async Task<Exception> Thrown(Func<Task> run)
    {
        try
        {
            await run();
        }
        catch (Exception error)
        {
            return error;
        }
        throw new Xunit.Sdk.XunitException("nothing was thrown");
    }

    public void Dispose()
    {
        lock (_clients)
        {
            foreach (var client in _clients)
            {
                client.Dispose();
            }
        }
        Server.Dispose();
        Broker.Dispose();
    }
}
