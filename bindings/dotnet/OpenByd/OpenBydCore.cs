using System.Text.Json;
using OpenByd.Models;

namespace OpenByd;

/// <summary>The state of the Rust runtime every client shares.</summary>
/// <param name="WorkerThreads">The runtime's worker threads.</param>
/// <param name="Started">Whether the runtime has started: after the first client or call.</param>
public sealed record RuntimeInfo(int WorkerThreads, bool Started);

/// <summary>
/// What needs no client: the envelope cipher, pyBYD's models by type, the runtime's settings.
/// </summary>
public static class OpenBydCore
{
    /// <summary>The version of openbyd's core.</summary>
    public static string Version => Wire.Call("version", new Args(), Wire.Str);

    /// <summary>The state of the Rust runtime every client shares.</summary>
    public static RuntimeInfo Runtime => Wire.Call("runtime", new Args(), ReadRuntime);

    /// <summary>
    /// Sets the worker threads of the Rust runtime (2 by default). Only before the runtime
    /// starts, which the first client does.
    /// </summary>
    /// <exception cref="DataException">The number is not positive, or the runtime has started.</exception>
    public static void ConfigureRuntime(int workerThreads) =>
        Wire.Call("configure", new Args().Add("worker_threads", (long)workerThreads), Wire.Any);

    /// <summary>Wraps text in a Bangcle envelope, as BYD's app does with every request.</summary>
    public static string EncodeEnvelope(string plaintext) =>
        Wire.Call("encode_envelope", new Args().Add("text", plaintext), Wire.Str);

    /// <summary>Wraps bytes in a Bangcle envelope, as BYD's app does with every request.</summary>
    public static string EncodeEnvelope(ReadOnlySpan<byte> plaintext) =>
        Wire.Call("encode_envelope", new Args().Add("hex", Convert.ToHexString(plaintext)), Wire.Str);

    /// <summary>Opens a Bangcle envelope and returns the plaintext bytes.</summary>
    /// <exception cref="BangcleException">The envelope cannot be opened.</exception>
    public static byte[] DecodeEnvelope(string envelope) =>
        Convert.FromHexString(Wire.Call("decode_envelope", new Args().Add("envelope", envelope), ReadHex));

    /// <summary>
    /// Parses a raw payload (a reply's <c>respondData</c>, an MQTT event's data) as one of
    /// pyBYD's reply models: <c>Model.model_validate(raw).model_dump(mode="json")</c>.
    /// </summary>
    /// <typeparam name="T">The model, such as <see cref="GpsInfo"/>.</typeparam>
    /// <param name="raw">The payload, a JSON object.</param>
    /// <param name="context">pyBYD's validation context, for the models that take one.</param>
    /// <exception cref="ValidationException">The payload does not validate.</exception>
    public static T ParseModel<T>(JsonElement raw, JsonElement? context = null)
        where T : class, IBydModel
    {
        var model = ModelCalls.Of<T>();
        var args = new Args().Add("name", model.Call)
            .AddValue<JsonElement>("raw", raw, Wire.WriteAny)
            .AddValue<JsonElement>("context", context, Wire.WriteAny);
        return Wire.Call("parse_model", args, (ref Utf8JsonReader reader) => ReadDump(ref reader, model.Read));
    }

    /// <summary>
    /// Parses a raw payload given as JSON text as one of pyBYD's reply models. See
    /// <see cref="ParseModel{T}(JsonElement, Nullable{JsonElement})"/>.
    /// </summary>
    /// <typeparam name="T">The model, such as <see cref="GpsInfo"/>.</typeparam>
    /// <param name="rawJson">The payload: the text of a JSON object.</param>
    /// <param name="contextJson">pyBYD's validation context as JSON text, for the models that take one.</param>
    /// <exception cref="ValidationException">The payload does not validate.</exception>
    /// <exception cref="DataException">An argument is not JSON.</exception>
    public static T ParseModel<T>(string rawJson, string? contextJson = null)
        where T : class, IBydModel
    {
        ArgumentNullException.ThrowIfNull(rawJson);
        var model = ModelCalls.Of<T>();
        var args = new Args().Add("name", model.Call).AddRaw("raw", rawJson).AddRaw("context", contextJson);
        return Wire.Call("parse_model", args, (ref Utf8JsonReader reader) => ReadDump(ref reader, model.Read));
    }

    /// <summary>The <c>dump</c> of what <c>parse_model</c> returns: <c>{"dump", "properties"}</c>.</summary>
    private static T ReadDump<T>(ref Utf8JsonReader reader, Reader<T> read)
        where T : class
    {
        T? dump = null;
        Wire.BeginObject(ref reader, "a parsed model");
        while (Wire.NextName(ref reader, out var field))
        {
            if (field == "dump")
            {
                dump = read(ref reader);
            }
            else
            {
                reader.Skip();
            }
        }
        return dump ?? throw new JsonException("the parsed model has no dump");
    }

    private static string ReadHex(ref Utf8JsonReader reader)
    {
        string hex = "";
        Wire.BeginObject(ref reader, "the plaintext");
        while (Wire.NextName(ref reader, out var field))
        {
            if (field == "hex")
            {
                hex = Wire.Str(ref reader);
            }
            else
            {
                reader.Skip();
            }
        }
        return hex;
    }

    private static RuntimeInfo ReadRuntime(ref Utf8JsonReader reader)
    {
        long workerThreads = 0;
        bool started = false;
        Wire.BeginObject(ref reader, "the runtime's state");
        while (Wire.NextName(ref reader, out var field))
        {
            switch (field)
            {
                case "worker_threads":
                    workerThreads = Wire.Int(ref reader);
                    break;
                case "started":
                    started = Wire.Bool(ref reader);
                    break;
                default:
                    reader.Skip();
                    break;
            }
        }
        return new RuntimeInfo(checked((int)workerThreads), started);
    }
}
