using System.Text.Json;

namespace OpenByd;

/// <summary>What an error is built from: its fields, whichever its class carries.</summary>
internal readonly record struct ErrorDetails(
    string Kind,
    string Message,
    string? Code = null,
    string? Endpoint = null,
    long? StatusCode = null,
    string? ErrorType = null,
    IReadOnlyList<string>? Loc = null);

/// <summary>Errors as they cross: the core's error JSON in, a callback's failure out.</summary>
internal static class Errors
{
    /// <summary>
    /// The exception for what the core reported: the class of its Python exception, or of
    /// the nearest parent this package knows, or the root.
    /// </summary>
    internal static OpenBydException FromJson(ReadOnlySpan<byte> json)
    {
        string kind = "";
        string message = "";
        string? code = null;
        string? endpoint = null;
        string? errorType = null;
        long? statusCode = null;
        IReadOnlyList<string> bases = [];
        IReadOnlyList<string>? loc = null;
        var reader = new Utf8JsonReader(json);
        reader.Read();
        Wire.BeginObject(ref reader, "an error");
        while (Wire.NextName(ref reader, out var name))
        {
            switch (name)
            {
                case "class":
                    kind = Wire.Str(ref reader);
                    break;
                case "bases":
                    bases = Wire.List(ref reader, Wire.Str);
                    break;
                case "message":
                    message = Wire.Str(ref reader);
                    break;
                case "code":
                    code = Wire.Opt(ref reader, Wire.Str);
                    break;
                case "endpoint":
                    endpoint = Wire.Opt(ref reader, Wire.Str);
                    break;
                case "status_code":
                    statusCode = Wire.OptValue(ref reader, Wire.Int);
                    break;
                case "error_type":
                    errorType = Wire.Opt(ref reader, Wire.Str);
                    break;
                case "loc":
                    loc = Wire.Opt(ref reader, static (ref Utf8JsonReader items) => Wire.List(ref items, Wire.Str));
                    break;
                default:
                    // A field of a newer core.
                    reader.Skip();
                    break;
            }
        }
        var details = new ErrorDetails(kind, message, code, endpoint, statusCode, errorType, loc);
        if (ErrorClasses.ByPythonName.TryGetValue(kind, out var make))
        {
            return make(details);
        }
        foreach (var parent in bases)
        {
            if (ErrorClasses.ByPythonName.TryGetValue(parent, out make))
            {
                return make(details);
            }
        }
        return new OpenBydException(details);
    }

    /// <summary>
    /// An error a callback threw, as the core takes it back: an openbyd error keeps its
    /// class and fields, anything else is named by its own type.
    /// </summary>
    internal static void Write(Utf8JsonWriter writer, Exception error)
    {
        writer.WriteStartObject();
        writer.WriteString("class", error is OpenBydException known ? known.Kind : error.GetType().Name);
        writer.WriteString("message", error.Message);
        switch (error)
        {
            case BydApiException api:
                writer.WriteString("code", api.Code);
                writer.WriteString("endpoint", api.Endpoint);
                break;
            case BydTransportException transport:
                writer.WriteString("endpoint", transport.Endpoint);
                Wire.Field(writer, "status_code", transport.StatusCode);
                break;
        }
        writer.WriteEndObject();
    }
}
