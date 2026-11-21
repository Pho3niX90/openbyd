using System.Text.Json;

namespace OpenByd.Testing;

/// <summary>The outcome of a parity call: an answer, or the Python exception class pyBYD raises.</summary>
public sealed record ParityOutcome
{
    /// <summary>The answer; null when the call failed.</summary>
    public JsonElement? Ok { get; init; }

    /// <summary>The name of the Python exception class; null when the call answered.</summary>
    public string? ErrorKind { get; init; }

    /// <summary>The failure's message, which is openbyd's wording; null when the call answered.</summary>
    public string? ErrorMessage { get; init; }
}

/// <summary>
/// The parity hook: the calls the repository's tests compare openbyd with pyBYD through.
/// pyBYD's answer for the same input is recorded in the repository's vectors. Not part of
/// the API an application uses.
/// </summary>
public static class Parity
{
    /// <summary>Runs one parity call of the core.</summary>
    /// <param name="name">The call's name, such as <c>hashing.md5_hex</c>.</param>
    /// <param name="argsJson">The call's arguments as JSON text, sent as they are.</param>
    public static ParityOutcome Call(string name, string argsJson) =>
        Wire.Call("parity_call", new Args().Add("name", name).AddRaw("args", argsJson), static (ref Utf8JsonReader reader) =>
        {
            var outcome = Wire.Any(ref reader);
            if (outcome.TryGetProperty("error", out var error))
            {
                return new ParityOutcome
                {
                    ErrorKind = error.GetProperty("kind").GetString(),
                    ErrorMessage = error.GetProperty("message").GetString(),
                };
            }
            return new ParityOutcome { Ok = outcome.GetProperty("ok") };
        });
}
