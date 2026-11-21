using System.Text.Json;
using System.Text.Json.Nodes;

namespace OpenByd.Testing;

/// <summary>
/// Whether the native library loaded has the fake server and the test broker: it must be
/// built with the cargo feature <c>testing</c>, which released libraries are not.
/// </summary>
public static class TestSupport
{
    /// <summary>Whether <see cref="FakeServer"/> and <see cref="TestBroker"/> can be used.</summary>
    public static bool IsAvailable => Wire.Call("has_testing", new Args(), Wire.Bool);

    internal static T Call<T>(string name, Args args, Reader<T> read) => Wire.TestingCall(name, args, read);

    internal static void Call(string name, Args args) => Wire.TestingCall(name, args, Wire.Any);

    internal static void WriteNode(Utf8JsonWriter writer, JsonNode value) => value.WriteTo(writer);
}

/// <summary>An account the fake server knows.</summary>
public sealed class Account
{
    /// <summary>The account's user name.</summary>
    public required string Username { get; init; }

    /// <summary>The account's password.</summary>
    public required string Password { get; init; }

    /// <summary>The account's user ID; made up when null.</summary>
    public string? UserId { get; init; }

    /// <summary>The VINs the account reaches as a guest.</summary>
    public IReadOnlyList<string>? GuestVins { get; init; }

    /// <summary>Whether the server insists on the <c>userType</c> a guest's requests carry. Default <c>true</c>.</summary>
    public bool? UserTypeRule { get; init; }

    internal Args AddTo(Args args) => args
        .Add("username", Username)
        .Add("password", Password)
        .Add("user_id", UserId)
        .Add("guest_vins", GuestVins, static (writer, vins) => Wire.WriteList(writer, vins, Wire.WriteStr))
        .Add("user_type_rule", UserTypeRule);

    internal static void Write(Utf8JsonWriter writer, Account account)
    {
        writer.WriteStartObject();
        Wire.Field(writer, "username", account.Username);
        Wire.Field(writer, "password", account.Password);
        Wire.Field(writer, "user_id", account.UserId);
        Wire.Field(writer, "guest_vins", account.GuestVins, static (inner, vins) => Wire.WriteList(inner, vins, Wire.WriteStr));
        Wire.Field(writer, "user_type_rule", account.UserTypeRule);
        writer.WriteEndObject();
    }
}

/// <summary>What the fake server answers to one request.</summary>
public sealed class Reply
{
    private readonly string _key;
    private readonly JsonNode _value;
    private readonly string? _message;
    private readonly string? _body;
    private readonly double? _delay;
    private readonly string? _cookie;

    private Reply(string key, JsonNode value, string? message = null, string? body = null, double? delay = null, string? cookie = null)
    {
        _key = key;
        _value = value;
        _message = message;
        _body = body;
        _delay = delay;
        _cookie = cookie;
    }

    /// <summary>Success: code <c>0</c> with <paramref name="data"/> as the encrypted <c>respondData</c>.</summary>
    public static Reply Data(JsonNode data) => new("data", data);

    /// <summary>An API error with this code.</summary>
    public static Reply Code(string code, string? message = null) => new("code", code, message: message);

    /// <summary>This outer reply as it is, inside the envelope only.</summary>
    public static Reply Outer(JsonNode outer) => new("outer", outer);

    /// <summary>An HTTP status other than 200, with this body.</summary>
    public static Reply Status(int status, string? body = null) => new("status", status, body: body);

    /// <summary>Closes the connection without answering.</summary>
    public static Reply Drop { get; } = new("drop", true);

    /// <summary>What the server answers without a handler.</summary>
    public static Reply Default { get; } = new("default", true);

    /// <summary>The same reply, given after this many seconds.</summary>
    public Reply WithDelay(double seconds) => new(_key, _value, _message, _body, seconds, _cookie);

    /// <summary>The same reply, with this <c>Set-Cookie</c> header.</summary>
    public Reply WithCookie(string header) => new(_key, _value, _message, _body, _delay, header);

    internal static void Write(Utf8JsonWriter writer, Reply reply)
    {
        writer.WriteStartObject();
        writer.WritePropertyName(reply._key);
        reply._value.WriteTo(writer);
        Wire.Field(writer, "message", reply._message);
        Wire.Field(writer, "body", reply._body);
        Wire.Field(writer, "delay", reply._delay);
        Wire.Field(writer, "cookie", reply._cookie);
        writer.WriteEndObject();
    }
}

/// <summary>One request the fake server received, decoded.</summary>
public sealed record RecordedRequest
{
    /// <summary>The endpoint the request went to.</summary>
    public required string Endpoint { get; init; }

    /// <summary>The request's headers, in order: name and value.</summary>
    public required IReadOnlyList<KeyValuePair<string, string>> Headers { get; init; }

    /// <summary>The cookies the request carried: name and value.</summary>
    public required IReadOnlyList<KeyValuePair<string, string>> Cookies { get; init; }

    /// <summary>The request's body as it was sent.</summary>
    public required string Body { get; init; }

    /// <summary>The outer payload, when the envelope opened.</summary>
    public required JsonElement? Outer { get; init; }

    /// <summary>The decrypted inner payload; JSON null when it did not decrypt.</summary>
    public required JsonElement Inner { get; init; }

    /// <summary>The account the request was verified as.</summary>
    public required string? Username { get; init; }

    /// <summary>The API code answered; null for an HTTP-level reply.</summary>
    public required string? Code { get; init; }

    /// <summary>The HTTP status answered; null when the connection was dropped.</summary>
    public required long? Status { get; init; }

    internal static RecordedRequest Read(ref Utf8JsonReader reader)
    {
        string endpoint = "";
        string body = "";
        IReadOnlyList<KeyValuePair<string, string>> headers = [];
        IReadOnlyList<KeyValuePair<string, string>> cookies = [];
        JsonElement? outer = null;
        JsonElement inner = default;
        string? username = null;
        string? code = null;
        long? status = null;
        Wire.BeginObject(ref reader, "a request");
        while (Wire.NextName(ref reader, out var field))
        {
            switch (field)
            {
                case "endpoint":
                    endpoint = Wire.Str(ref reader);
                    break;
                case "headers":
                    headers = Wire.List(ref reader, ReadPair);
                    break;
                case "cookies":
                    cookies = Wire.List(ref reader, ReadPair);
                    break;
                case "body":
                    body = Wire.Str(ref reader);
                    break;
                case "outer":
                    outer = Wire.OptValue(ref reader, Wire.Any);
                    break;
                case "inner":
                    inner = Wire.Any(ref reader);
                    break;
                case "username":
                    username = Wire.Opt(ref reader, Wire.Str);
                    break;
                case "code":
                    code = Wire.Opt(ref reader, Wire.Str);
                    break;
                case "status":
                    status = Wire.OptValue(ref reader, Wire.Int);
                    break;
                default:
                    reader.Skip();
                    break;
            }
        }
        return new RecordedRequest
        {
            Endpoint = endpoint,
            Headers = headers,
            Cookies = cookies,
            Body = body,
            Outer = outer,
            Inner = inner,
            Username = username,
            Code = code,
            Status = status,
        };
    }

    private static KeyValuePair<string, string> ReadPair(ref Utf8JsonReader reader)
    {
        var pair = Wire.List(ref reader, Wire.Str);
        return pair.Count == 2
            ? new KeyValuePair<string, string>(pair[0], pair[1])
            : throw new JsonException("a header is a name and a value");
    }
}

/// <summary>
/// A fake BYD server on 127.0.0.1 that speaks the real wire protocol, for tests. It needs a
/// native library built with the feature <c>testing</c> (see <see cref="TestSupport"/>).
/// </summary>
/// <example>
/// <code>
/// using var server = new FakeServer(new Account { Username = "user@example.com", Password = "secret" });
/// server.Reply("/control/getStatusNow", Reply.Data(new JsonObject { ["acSwitch"] = 1 }));
/// using var client = new Client(new BydConfig
/// {
///     Username = "user@example.com", Password = "secret", BaseUrl = server.Url,
/// });
/// </code>
/// </example>
public sealed class FakeServer : IDisposable
{
    private readonly long _server;

    /// <summary>Starts a server that knows these accounts.</summary>
    /// <exception cref="DataException">The native library was built without the feature <c>testing</c>.</exception>
    public FakeServer(params Account[] accounts)
    {
        var args = new Args().Add<IReadOnlyList<Account>>(
            "accounts", accounts, static (writer, items) => Wire.WriteList(writer, items, Account.Write));
        (_server, Url) = TestSupport.Call("server.start", args, static (ref Utf8JsonReader reader) =>
        {
            long server = 0;
            string url = "";
            Wire.BeginObject(ref reader, "a server");
            while (Wire.NextName(ref reader, out var field))
            {
                switch (field)
                {
                    case "server":
                        server = Wire.Int(ref reader);
                        break;
                    case "url":
                        url = Wire.Str(ref reader);
                        break;
                    default:
                        reader.Skip();
                        break;
                }
            }
            return (server, url);
        });
    }

    /// <summary>The server's base URL, for a client's <c>BaseUrl</c>.</summary>
    public string Url { get; }

    private Args ForServer() => new Args().Add("server", _server);

    /// <summary>Makes the server know another account.</summary>
    public void AddAccount(Account account) => TestSupport.Call("server.add_account", account.AddTo(ForServer()));

    /// <summary>Answers every request to <paramref name="endpoint"/> with <paramref name="reply"/>.</summary>
    public void Reply(string endpoint, Reply reply) =>
        TestSupport.Call("server.reply", ForServer().Add("endpoint", endpoint).Add("reply", reply, Testing.Reply.Write));

    /// <summary>Answers the next requests to <paramref name="endpoint"/> with <paramref name="replies"/>, one each.</summary>
    public void Script(string endpoint, params Reply[] replies) =>
        TestSupport.Call("server.script", ForServer().Add("endpoint", endpoint).Add<IReadOnlyList<Reply>>(
            "replies", replies, static (writer, items) => Wire.WriteList(writer, items, Testing.Reply.Write)));

    /// <summary>The requests received, to one endpoint or to all.</summary>
    public IReadOnlyList<RecordedRequest> Requests(string? endpoint = null) =>
        TestSupport.Call("server.requests", ForServer().Add("endpoint", endpoint),
            static (ref Utf8JsonReader reader) => Wire.List(ref reader, RecordedRequest.Read));

    /// <summary>Forgets the requests received so far.</summary>
    public void ClearRequests() => TestSupport.Call("server.clear_requests", ForServer());

    /// <summary>Makes the server refuse every session of an account, as an expired one.</summary>
    public void ExpireSessions(string username) =>
        TestSupport.Call("server.expire_sessions", ForServer().Add("username", username));

    /// <summary>The user ID of an account, or null when the server does not know it.</summary>
    public string? UserId(string username) =>
        TestSupport.Call("server.user_id", ForServer().Add("username", username),
            static (ref Utf8JsonReader reader) => Wire.Opt(ref reader, Wire.Str));

    /// <summary>Stops the server.</summary>
    public void Dispose() => TestSupport.Call("server.stop", ForServer());
}
