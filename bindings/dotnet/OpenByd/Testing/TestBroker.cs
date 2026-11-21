using System.Text.Json;
using System.Text.Json.Nodes;
using OpenByd.Models;

namespace OpenByd.Testing;

/// <summary>
/// A TLS MQTT broker on 127.0.0.1, for tests. It needs a native library built with the
/// feature <c>testing</c> (see <see cref="TestSupport"/>), and only this package's own tests
/// can make a client trust its certificate.
/// </summary>
public sealed class TestBroker : IDisposable
{
    private readonly long _broker;

    /// <summary>Starts a broker.</summary>
    /// <exception cref="DataException">The native library was built without the feature <c>testing</c>.</exception>
    public TestBroker()
    {
        (_broker, Address, Port) = TestSupport.Call("broker.start", new Args(), static (ref Utf8JsonReader reader) =>
        {
            long broker = 0;
            string address = "";
            long port = 0;
            Wire.BeginObject(ref reader, "a broker");
            while (Wire.NextName(ref reader, out var field))
            {
                switch (field)
                {
                    case "broker":
                        broker = Wire.Int(ref reader);
                        break;
                    case "address":
                        address = Wire.Str(ref reader);
                        break;
                    case "port":
                        port = Wire.Int(ref reader);
                        break;
                    default:
                        reader.Skip();
                        break;
                }
            }
            return (broker, address, (int)port);
        });
    }

    /// <summary>
    /// The address as BYD's broker lookup gives it: the fake server answers
    /// <c>/app/emqAuth/getEmqBrokerIp</c> with <c>{"emqBorker": Address}</c>.
    /// </summary>
    public string Address { get; }

    /// <summary>The port the broker listens on.</summary>
    public int Port { get; }

    private Args ForBroker() => new Args().Add("broker", _broker);

    /// <summary>
    /// Sends <paramref name="body"/> to the account of <paramref name="session"/> as BYD does:
    /// on its topic, encrypted with the session's content key. Returns how many connections
    /// it reached.
    /// </summary>
    public long Push(SessionTokens session, JsonNode body) =>
        TestSupport.Call(
            "broker.push",
            ForBroker().Add("session", session, SessionTokensJson.WriteValue).Add("body", body, TestSupport.WriteNode),
            Wire.Int);

    /// <summary>Connections subscribed to <paramref name="topic"/>.</summary>
    public long Subscribers(string topic) =>
        TestSupport.Call("broker.subscribers", ForBroker().Add("topic", topic), Wire.Int);

    /// <summary>Connections still open on the broker's side.</summary>
    public long Open => TestSupport.Call("broker.open", ForBroker(), Wire.Int);

    /// <summary>How many connection attempts the broker has seen.</summary>
    public long Connects => TestSupport.Call("broker.connects", ForBroker(), Wire.Int);

    /// <summary>Refuses the next <paramref name="count"/> connection attempts.</summary>
    public void RefuseNext(int count) => TestSupport.Call("broker.refuse_next", ForBroker().Add("count", (long)count));

    /// <summary>Drops every connection.</summary>
    public void DropAll() => TestSupport.Call("broker.drop_all", ForBroker());

    /// <summary>Stops the broker.</summary>
    public void Dispose() => TestSupport.Call("broker.stop", ForBroker());
}
