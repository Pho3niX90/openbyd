using System.Buffers;
using System.Globalization;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace OpenByd;

/// <summary>Reads one JSON value. On entry the reader is on the value's first token; on exit on its last.</summary>
internal delegate T Reader<T>(ref Utf8JsonReader reader);

/// <summary>Writes one JSON value.</summary>
internal delegate void Writer<in T>(Utf8JsonWriter writer, T value);

/// <summary>
/// A generated reader and writer as a converter of <c>System.Text.Json</c>, so that
/// serialising a model anywhere gives pyBYD's JSON and reading one is as strict as here.
/// </summary>
internal abstract class WireConverter<T>(Reader<T> read, Writer<T> write) : JsonConverter<T>
{
    public override T Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options) =>
        read(ref reader);

    public override void Write(Utf8JsonWriter writer, T value, JsonSerializerOptions options) =>
        write(writer, value);
}

/// <summary>
/// What crosses between C# and the core: JSON text. The generated readers and writers
/// (<c>Generated/Json.g.cs</c>) are built from these.
/// </summary>
/// <remarks>
/// Reading is strict, because the core's JSON is exactly pyBYD's: a value of another type,
/// a null where the type has none, an unknown or a missing field all fail with
/// <see cref="JsonException"/>. A float JSON cannot hold crosses as one of the strings
/// <c>"NaN"</c>, <c>"Infinity"</c> and <c>"-Infinity"</c>, only where the type says float.
/// </remarks>
internal static class Wire
{
    /// <summary>serde_json nests no deeper than 128, so nothing the core writes is refused.</summary>
    private const int MaxDepth = 256;

    private static readonly JsonReaderOptions ReaderOptions = new() { MaxDepth = MaxDepth };

    // -- reading -------------------------------------------------------------

    private static JsonException Unexpected(ref Utf8JsonReader reader, string expected) =>
        new($"expected {expected}, found {reader.TokenType}");

    /// <summary>The failure for a value that is not one of an enum's.</summary>
    internal static JsonException NotAMember(object value, string type) => new($"{value} is not a valid {type}");

    private static void Advance(ref Utf8JsonReader reader)
    {
        if (!reader.Read())
        {
            throw new JsonException("the JSON ends early");
        }
    }

    internal static bool IsNull(ref Utf8JsonReader reader) => reader.TokenType == JsonTokenType.Null;

    internal static string Str(ref Utf8JsonReader reader) =>
        reader.TokenType == JsonTokenType.String ? reader.GetString()! : throw Unexpected(ref reader, "a string");

    internal static long Int(ref Utf8JsonReader reader) =>
        reader.TokenType == JsonTokenType.Number && reader.TryGetInt64(out var value)
            ? value
            : throw Unexpected(ref reader, "an integer");

    internal static double Float(ref Utf8JsonReader reader)
    {
        if (reader.TokenType == JsonTokenType.Number)
        {
            return reader.GetDouble();
        }
        if (reader.TokenType == JsonTokenType.String)
        {
            if (reader.ValueTextEquals("NaN"u8))
            {
                return double.NaN;
            }
            if (reader.ValueTextEquals("Infinity"u8))
            {
                return double.PositiveInfinity;
            }
            if (reader.ValueTextEquals("-Infinity"u8))
            {
                return double.NegativeInfinity;
            }
        }
        throw Unexpected(ref reader, "a number");
    }

    internal static bool Bool(ref Utf8JsonReader reader) => reader.TokenType switch
    {
        JsonTokenType.True => true,
        JsonTokenType.False => false,
        _ => throw Unexpected(ref reader, "a boolean"),
    };

    /// <summary>Any JSON value, kept as it was written.</summary>
    internal static JsonElement Any(ref Utf8JsonReader reader) => JsonElement.ParseValue(ref reader);

    internal static T? Opt<T>(ref Utf8JsonReader reader, Reader<T> read)
        where T : class => IsNull(ref reader) ? null : read(ref reader);

    internal static T? OptValue<T>(ref Utf8JsonReader reader, Reader<T> read)
        where T : struct => IsNull(ref reader) ? null : read(ref reader);

    internal static IReadOnlyList<T> List<T>(ref Utf8JsonReader reader, Reader<T> read)
    {
        if (reader.TokenType != JsonTokenType.StartArray)
        {
            throw Unexpected(ref reader, "a list");
        }
        var items = new List<T>();
        while (true)
        {
            Advance(ref reader);
            if (reader.TokenType == JsonTokenType.EndArray)
            {
                return items;
            }
            items.Add(read(ref reader));
        }
    }

    /// <summary>A JSON object with any values: pyBYD's <c>dict[str, Any]</c>, in the order it was written.</summary>
    internal static IReadOnlyDictionary<string, JsonElement> Object(ref Utf8JsonReader reader)
    {
        if (reader.TokenType != JsonTokenType.StartObject)
        {
            throw Unexpected(ref reader, "an object");
        }
        // One document for the whole object; its values share it.
        var fields = new Dictionary<string, JsonElement>();
        foreach (var property in JsonElement.ParseValue(ref reader).EnumerateObject())
        {
            fields[property.Name] = property.Value;
        }
        return fields;
    }

    internal static IReadOnlyDictionary<string, T> Map<T>(ref Utf8JsonReader reader, Reader<T> read)
    {
        BeginObject(ref reader, "a map");
        var entries = new Dictionary<string, T>();
        while (NextName(ref reader, out var key))
        {
            entries[key] = read(ref reader);
        }
        return entries;
    }

    /// <summary>A map whose keys are integers written as strings.</summary>
    internal static IReadOnlyDictionary<int, T> IntMap<T>(ref Utf8JsonReader reader, Reader<T> read)
    {
        BeginObject(ref reader, "a map");
        var entries = new Dictionary<int, T>();
        while (NextName(ref reader, out var key))
        {
            if (!int.TryParse(key, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var number))
            {
                throw new JsonException($"the key {key} is not an integer");
            }
            entries[number] = read(ref reader);
        }
        return entries;
    }

    internal static void BeginObject(ref Utf8JsonReader reader, string what)
    {
        if (reader.TokenType != JsonTokenType.StartObject)
        {
            throw Unexpected(ref reader, what);
        }
    }

    /// <summary>
    /// Moves to the next field of an object: its name, with the reader left on its value.
    /// False at the object's end.
    /// </summary>
    internal static bool NextName(ref Utf8JsonReader reader, out string name)
    {
        Advance(ref reader);
        if (reader.TokenType == JsonTokenType.EndObject)
        {
            name = "";
            return false;
        }
        name = reader.GetString()!;
        Advance(ref reader);
        return true;
    }

    /// <summary>Notes that field <paramref name="index"/> of a struct was read; a second time is an error.</summary>
    internal static void Mark(Span<bool> seen, int index, string name)
    {
        if (seen[index])
        {
            throw new JsonException($"the field {name} is given twice");
        }
        seen[index] = true;
    }

    internal static JsonException UnknownField(string type, string name) => new($"{type} has no field {name}");

    /// <summary>Fails unless every field of a struct was read.</summary>
    internal static void Complete(ReadOnlySpan<bool> seen, string[] names, string type)
    {
        var missing = seen.IndexOf(false);
        if (missing >= 0)
        {
            throw new JsonException($"{type} lacks the field {names[missing]}");
        }
    }

    /// <summary>JSON text as a value.</summary>
    internal static T Decode<T>(ReadOnlySpan<byte> json, Reader<T> read)
    {
        var reader = new Utf8JsonReader(json, ReaderOptions);
        Advance(ref reader);
        return read(ref reader);
    }

    // -- writing -------------------------------------------------------------

    internal static void WriteStr(Utf8JsonWriter writer, string value) => writer.WriteStringValue(value);

    internal static void WriteInt(Utf8JsonWriter writer, long value) => writer.WriteNumberValue(value);

    internal static void WriteBool(Utf8JsonWriter writer, bool value) => writer.WriteBooleanValue(value);

    /// <summary>
    /// A float as the core writes it: always recognisable as a float (<c>1.0</c>, never
    /// <c>1</c>), and the three strings for what JSON cannot hold.
    /// </summary>
    internal static void WriteFloat(Utf8JsonWriter writer, double value)
    {
        if (double.IsNaN(value))
        {
            writer.WriteStringValue("NaN"u8);
        }
        else if (double.IsInfinity(value))
        {
            writer.WriteStringValue(value > 0 ? "Infinity"u8 : "-Infinity"u8);
        }
        else
        {
            WriteFinite(writer, value);
        }
    }

    private static void WriteFinite(Utf8JsonWriter writer, double value)
    {
        // The shortest text that reads back as the same double: "52", "0.1", "1E+20".
        Span<byte> text = stackalloc byte[40];
        value.TryFormat(text, out var length, "R", CultureInfo.InvariantCulture);
        if (text[..length].IndexOfAny((byte)'.', (byte)'E') < 0)
        {
            ".0"u8.CopyTo(text[length..]);
            length += 2;
        }
        writer.WriteRawValue(text[..length], skipInputValidation: true);
    }

    internal static void WriteAny(Utf8JsonWriter writer, JsonElement value)
    {
        if (value.ValueKind == JsonValueKind.Undefined)
        {
            writer.WriteNullValue();
        }
        else
        {
            value.WriteTo(writer);
        }
    }

    internal static void WriteOpt<T>(Utf8JsonWriter writer, T? value, Writer<T> write)
        where T : class
    {
        if (value is null)
        {
            writer.WriteNullValue();
        }
        else
        {
            write(writer, value);
        }
    }

    internal static void WriteOptValue<T>(Utf8JsonWriter writer, T? value, Writer<T> write)
        where T : struct
    {
        if (value is null)
        {
            writer.WriteNullValue();
        }
        else
        {
            write(writer, value.Value);
        }
    }

    internal static void WriteList<T>(Utf8JsonWriter writer, IReadOnlyList<T> value, Writer<T> write)
    {
        writer.WriteStartArray();
        foreach (var item in value)
        {
            write(writer, item);
        }
        writer.WriteEndArray();
    }

    internal static void WriteObject(Utf8JsonWriter writer, IReadOnlyDictionary<string, JsonElement> value) =>
        WriteMap(writer, value, WriteAny);

    internal static void WriteMap<T>(Utf8JsonWriter writer, IReadOnlyDictionary<string, T> value, Writer<T> write)
    {
        writer.WriteStartObject();
        foreach (var (key, item) in value)
        {
            writer.WritePropertyName(key);
            write(writer, item);
        }
        writer.WriteEndObject();
    }

    internal static void WriteIntMap<T>(Utf8JsonWriter writer, IReadOnlyDictionary<int, T> value, Writer<T> write)
    {
        writer.WriteStartObject();
        foreach (var (key, item) in value)
        {
            writer.WritePropertyName(key.ToString(CultureInfo.InvariantCulture));
            write(writer, item);
        }
        writer.WriteEndObject();
    }

    // -- arguments -----------------------------------------------------------
    //
    // A field of something the caller passes. Null means "not given": the field is left
    // out, so the core applies its default.

    internal static void Field(Utf8JsonWriter writer, string name, string? value)
    {
        if (value is not null)
        {
            writer.WriteString(name, value);
        }
    }

    internal static void Field(Utf8JsonWriter writer, string name, long? value)
    {
        if (value is not null)
        {
            writer.WriteNumber(name, value.Value);
        }
    }

    internal static void Field(Utf8JsonWriter writer, string name, bool? value)
    {
        if (value is not null)
        {
            writer.WriteBoolean(name, value.Value);
        }
    }

    /// <summary>A number JSON cannot hold is refused, as pyBYD refuses it, instead of sent as something else.</summary>
    internal static void Field(Utf8JsonWriter writer, string name, double? value)
    {
        if (value is null)
        {
            return;
        }
        if (!double.IsFinite(value.Value))
        {
            throw new DataException(
                string.Create(CultureInfo.InvariantCulture, $"{name} must be a finite number, not {value.Value}"));
        }
        writer.WritePropertyName(name);
        WriteFinite(writer, value.Value);
    }

    internal static void Field<T>(Utf8JsonWriter writer, string name, T? value, Writer<T> write)
        where T : class
    {
        if (value is not null)
        {
            writer.WritePropertyName(name);
            write(writer, value);
        }
    }

    internal static void FieldValue<T>(Utf8JsonWriter writer, string name, T? value, Writer<T> write)
        where T : struct
    {
        if (value is not null)
        {
            writer.WritePropertyName(name);
            write(writer, value.Value);
        }
    }

    /// <summary>One value as NUL-terminated UTF-8 JSON, as the C library takes it.</summary>
    internal static byte[] Encode<T>(T value, Writer<T> write)
    {
        var buffer = new ArrayBufferWriter<byte>(256);
        using (var writer = new Utf8JsonWriter(buffer))
        {
            write(writer, value);
        }
        buffer.Write("\0"u8);
        return buffer.WrittenSpan.ToArray();
    }

    // -- the C library's strings ---------------------------------------------

    /// <summary>
    /// Takes a string the library returned as a value, and frees it. Throws the error it
    /// holds when <paramref name="ok"/> is 0.
    /// </summary>
    internal static unsafe T Take<T>(byte* text, int ok, Reader<T> read)
    {
        try
        {
            var json = MemoryMarshal.CreateReadOnlySpanFromNullTerminated(text);
            return ok != 0 ? Decode(json, read) : throw Errors.FromJson(json);
        }
        finally
        {
            Native.openbyd_string_free(text);
        }
    }

    /// <summary>Takes a string the library returned as the error it holds, and frees it.</summary>
    internal static unsafe OpenBydException TakeError(byte* text)
    {
        try
        {
            return Errors.FromJson(MemoryMarshal.CreateReadOnlySpanFromNullTerminated(text));
        }
        finally
        {
            Native.openbyd_string_free(text);
        }
    }

    /// <summary><see cref="Take{T}"/> for a call that returns nothing.</summary>
    internal static unsafe void Take(byte* text, int ok)
    {
        try
        {
            if (ok == 0)
            {
                throw Errors.FromJson(MemoryMarshal.CreateReadOnlySpanFromNullTerminated(text));
            }
        }
        finally
        {
            Native.openbyd_string_free(text);
        }
    }

    /// <summary>Runs a function of the core that needs no client.</summary>
    internal static unsafe T Call<T>(string function, Args args, Reader<T> read)
    {
        byte* text;
        int ok;
        fixed (byte* json = args.Finish())
        {
            text = Native.openbyd_call(function, json, out ok);
        }
        return Take(text, ok, read);
    }

    /// <summary>Runs a call of the fake server or the test broker.</summary>
    internal static unsafe T TestingCall<T>(string name, Args args, Reader<T> read)
    {
        byte* text;
        int ok;
        fixed (byte* json = args.Finish())
        {
            text = Native.openbyd_testing_call(name, json, out ok);
        }
        return Take(text, ok, read);
    }

}

/// <summary>
/// The arguments of one call: a JSON object written field by field. A null value is left
/// out, which the core reads as "not given".
/// </summary>
internal sealed class Args
{
    private readonly ArrayBufferWriter<byte> _buffer = new(256);
    private bool _finished;

    internal Args()
    {
        Writer = new Utf8JsonWriter(_buffer);
        Writer.WriteStartObject();
    }

    /// <summary>The writer, inside the arguments' object.</summary>
    internal Utf8JsonWriter Writer { get; }

    internal Args Add(string name, string? value)
    {
        Wire.Field(Writer, name, value);
        return this;
    }

    internal Args Add(string name, long? value)
    {
        Wire.Field(Writer, name, value);
        return this;
    }

    internal Args Add(string name, bool? value)
    {
        Wire.Field(Writer, name, value);
        return this;
    }

    internal Args Add(string name, double? value)
    {
        Wire.Field(Writer, name, value);
        return this;
    }

    internal Args Add<T>(string name, T? value, Writer<T> write)
        where T : class
    {
        Wire.Field(Writer, name, value, write);
        return this;
    }

    internal Args AddValue<T>(string name, T? value, Writer<T> write)
        where T : struct
    {
        Wire.FieldValue(Writer, name, value, write);
        return this;
    }

    /// <summary>
    /// JSON text sent as it is, so its numbers cross exactly as written. It is not checked
    /// here: the core refuses arguments that are not JSON.
    /// </summary>
    internal Args AddRaw(string name, string? json)
    {
        if (json is not null)
        {
            Writer.WritePropertyName(name);
            Writer.WriteRawValue(json, skipInputValidation: true);
        }
        return this;
    }

    /// <summary>The arguments as NUL-terminated UTF-8, as the C library takes them.</summary>
    internal ReadOnlySpan<byte> Finish()
    {
        if (!_finished)
        {
            Writer.WriteEndObject();
            Writer.Dispose();
            _buffer.Write("\0"u8);
            _finished = true;
        }
        return _buffer.WrittenSpan;
    }
}
