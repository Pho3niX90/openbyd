using System.Text.Json;
using OpenByd.Models;

namespace OpenByd;

/// <summary>
/// The options of a <see cref="Client"/> besides pyBYD's config: its callbacks and openbyd's
/// own settings. Every member is optional.
/// </summary>
/// <remarks>
/// Callbacks never run on the core's threads: each client hands its events, in order, to one
/// reader on the thread pool, so a callback that blocks holds back the ones behind it. A
/// callback that throws is reported to <see cref="OnCallbackError"/> and does not affect the
/// client.
/// </remarks>
public sealed class ClientOptions
{
    /// <summary>Every MQTT <c>vehicleInfo</c> push, parsed: the VIN and the realtime data.</summary>
    public Action<string, VehicleRealtimeData>? OnVehicleInfo { get; init; }

    /// <summary>
    /// Every MQTT event that names a VIN: the event's name (such as <c>vehicleInfo</c>), the
    /// VIN and the event's raw <c>respondData</c>.
    /// </summary>
    public Action<string, string, IReadOnlyDictionary<string, JsonElement>>? OnMqttEvent { get; init; }

    /// <summary>Every remote-control acknowledgement.</summary>
    public Action<CommandAckEvent>? OnCommandAck { get; init; }

    /// <summary>A command was registered, matched, expired or acknowledged without a match.</summary>
    public Action<CommandLifecycleEvent>? OnCommandLifecycle { get; init; }

    /// <summary>Every accepted MQTT connection.</summary>
    public Action? OnMqttConnect { get; init; }

    /// <summary>Every request the client sends, with its outcome and duration.</summary>
    public Action<RequestRecord>? RequestObserver { get; init; }

    /// <summary>How long a command waits for its acknowledgement, in seconds. Default <c>300</c>.</summary>
    public double? CommandAckTtlSeconds { get; init; }

    /// <summary>Attempts per remote command. Default <c>3</c>.</summary>
    public int? CommandRetries { get; init; }

    /// <summary>Whether a command is sent, given the command gate's verdict. Default <see cref="OpenByd.GatePolicy.Strict"/>.</summary>
    public GatePolicy? GatePolicy { get; init; }

    /// <summary>
    /// Returns the <c>commandPwd</c> to send (empty for none) for the PIN a call was given and
    /// the config's <c>ControlPin</c>, in place of pyBYD's rule (32 hex digits are a digest,
    /// anything else is hashed). If it throws, pyBYD's rule applies.
    /// </summary>
    public Func<string?, string?, string>? PinResolver { get; init; }

    /// <summary>What to do when a new session is needed. Default <see cref="ReloginPolicy.Password"/>.</summary>
    public ReloginPolicy? Relogin { get; init; }

    /// <summary>
    /// Hears about a callback that threw: the name of its option (<c>OnVehicleInfo</c>,
    /// <c>GatePolicy</c>, <c>OnStateChanged</c>, ...) and what it threw. By default the
    /// failure is written to <see cref="System.Diagnostics.Trace"/> as a warning.
    /// </summary>
    public Action<string, Exception>? OnCallbackError { get; init; }

    /// <summary>
    /// Makes the client trust the certificate of the MQTT test broker. For this package's own
    /// tests, with a library built with the feature <c>testing</c>.
    /// </summary>
    internal bool TestTls { get; init; }
}

/// <summary>
/// Whether a command is sent, given the command gate's verdict: <see cref="Strict"/>,
/// <see cref="AllowMissingData"/>, or a function of your own (<see cref="Custom"/>).
/// </summary>
public sealed class GatePolicy
{
    private GatePolicy(string wire, Func<string, CommandGateVerdict, bool>? decide)
    {
        WireName = wire;
        Decide = decide;
    }

    /// <summary>Only when the gate says the command is supported. pyBYD's behaviour, and the default.</summary>
    public static GatePolicy Strict { get; } = new("strict", null);

    /// <summary>
    /// Also when the car's capability data is incomplete (the gate's reasons
    /// <c>function_no_missing</c> and <c>learn_info_missing</c>).
    /// </summary>
    public static GatePolicy AllowMissingData { get; } = new("allow_missing_data", null);

    /// <summary>
    /// A function decides from the VIN and the gate's verdict. If it throws, or takes longer
    /// than 30 seconds, the verdict stands.
    /// </summary>
    public static GatePolicy Custom(Func<string, CommandGateVerdict, bool> decide)
    {
        ArgumentNullException.ThrowIfNull(decide);
        return new GatePolicy("host", decide);
    }

    /// <summary>The policy's name in the core's options.</summary>
    internal string WireName { get; }

    internal Func<string, CommandGateVerdict, bool>? Decide { get; }
}

/// <summary>
/// What a client does when it needs a new session: <see cref="Password"/>,
/// <see cref="Never"/>, or a function of your own (<see cref="Custom"/>).
/// </summary>
public sealed class ReloginPolicy
{
    private ReloginPolicy(string wire, Func<Task<SessionTokens>>? handler)
    {
        WireName = wire;
        Handler = handler;
    }

    /// <summary>Log in again with the password. pyBYD's behaviour, and the default.</summary>
    public static ReloginPolicy Password { get; } = new("password", null);

    /// <summary>
    /// Never log in: calls fail with <see cref="BydSessionExpiredException"/> (code
    /// <c>relogin_disabled</c>) until the application imports a session.
    /// </summary>
    public static ReloginPolicy Never { get; } = new("never", null);

    /// <summary>
    /// A function produces the new session (as <see cref="Client.ExportSession"/> gives it).
    /// What it throws is what the call that needed the session throws.
    /// </summary>
    public static ReloginPolicy Custom(Func<Task<SessionTokens>> handler)
    {
        ArgumentNullException.ThrowIfNull(handler);
        return new ReloginPolicy("host", handler);
    }

    /// <summary>The policy's name in the core's options.</summary>
    internal string WireName { get; }

    internal Func<Task<SessionTokens>>? Handler { get; }
}
