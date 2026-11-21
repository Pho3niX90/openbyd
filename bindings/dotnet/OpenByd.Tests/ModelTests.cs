using System.Text.Json;
using OpenByd.Models;

namespace OpenByd.Tests;

/// <summary>The generated types by themselves: strict reading, enums by their wire values, errors by class.</summary>
public sealed class ModelTests
{
    private const string Gps = """
        {"raw": {}, "latitude": 52.0, "longitude": 4.0, "speed": null, "direction": null,
         "gps_timestamp": null, "request_serial": null}
        """;

    [Fact]
    public void AModelReadsOnlyItsOwnJson()
    {
        var gps = JsonSerializer.Deserialize<GpsInfo>(Gps);
        Assert.Equal((52.0, 4.0), (gps?.Latitude, gps?.Longitude));

        // An unknown field, a missing field, a field given twice.
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<GpsInfo>(Gps.Replace("\"speed\"", "\"velocity\"")));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<GpsInfo>(Gps.Replace("\"speed\": null,", "")));
        Assert.Throws<JsonException>(
            () => JsonSerializer.Deserialize<GpsInfo>(Gps.Replace("\"speed\": null,", "\"speed\": null, \"speed\": 1.0,")));
        // Null where the type has none, and a value of another type.
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<GpsInfo>(Gps.Replace("\"raw\": {}", "\"raw\": null")));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<GpsInfo>(Gps.Replace("52.0", "\"52.0\"")));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<GpsInfo>(Gps.Replace("\"request_serial\": null", "\"request_serial\": 7")));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<GpsInfo>("[]"));

        const string diagnostics = """
            {"raw": {}, "pending": 1, "matched": 2, "expired": 3, "uncorrelated": 4, "pending_by_vin": {"TESTVIN0000000001": 1}}
            """;
        var counters = JsonSerializer.Deserialize<CommandAckDiagnostics>(diagnostics);
        Assert.Equal(1, counters?.PendingByVin["TESTVIN0000000001"]);
        // An integer is not a float that happens to be whole, and never null.
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<CommandAckDiagnostics>(diagnostics.Replace("\"pending\": 1", "\"pending\": 1.0")));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<CommandAckDiagnostics>(diagnostics.Replace("\"pending\": 1", "\"pending\": null")));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<CommandAckDiagnostics>(diagnostics.Replace("\": 1}", "\": null}")));
    }

    [Fact]
    public void EnumsCrossAsTheirWireValuesAndNoOther()
    {
        Assert.Equal(-1, (int)EnergyType.Unknown);
        Assert.Equal(15, (int)ChargingState.Connected);
        Assert.Equal("2", JsonSerializer.Serialize(EnergyType.Hybrid));
        Assert.Equal(EnergyType.Ev, JsonSerializer.Deserialize<EnergyType>("0"));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<EnergyType>("7"));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<EnergyType>("\"0\""));
        Assert.Null(JsonSerializer.Deserialize<EnergyType?>("null"));

        Assert.Equal("\"rear_left\"", JsonSerializer.Serialize(SeatPosition.RearLeft));
        Assert.Equal(SeatPosition.RearLeft, JsonSerializer.Deserialize<SeatPosition>("\"rear_left\""));
        Assert.Equal("\"LOCKDOOR\"", JsonSerializer.Serialize(RemoteCommand.Lock));
        Assert.Equal(RemoteCommand.OpenTrunk, JsonSerializer.Deserialize<RemoteCommand>("\"OPENTRUNK\""));
        Assert.Equal(CommandLifecycleStatus.Uncorrelated, JsonSerializer.Deserialize<CommandLifecycleStatus>("\"uncorrelated\""));
        Assert.Equal("\"ventilation\"", JsonSerializer.Serialize(SeatMode.Ventilation));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<SeatPosition>("\"boot\""));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<SeatPosition>("\"RearLeft\""));
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<SeatPosition>("0"));
        Assert.Throws<DataException>(() => JsonSerializer.Serialize((SeatPosition)99));
    }

    [Fact]
    public void ExceptionsArePyBydsHierarchy()
    {
        var expired = new BydSessionExpiredException("expired", code: "1005", endpoint: "/x");
        Assert.True(expired is BydAuthenticationException and BydApiException and BydException and OpenBydException);
        Assert.Equal(("BydSessionExpiredError", "1005", "/x", "expired"), (expired.Kind, expired.Code, expired.Endpoint, expired.Message));
        Assert.Equal("BydTransportError", new BydTransportException("down", statusCode: 503).Kind);
        Assert.Null(new BydTransportException("down").StatusCode);
        Assert.Equal("ValueError", new DataException("bad").Kind);
        Assert.Equal("TypeError", new DataException("bad", kind: "TypeError").Kind);
        var invalid = new ValidationException("bad");
        Assert.True(invalid is DataException);
        Assert.Equal(("ValidationError", "", 0), (invalid.Kind, invalid.ErrorType, invalid.Loc.Count));
        Assert.True(new BangcleTableLoadException("x") is BangcleException and BydCryptoException and BydException);
        Assert.False(typeof(BydException).IsAssignableFrom(typeof(CancelledException)));
        Assert.Equal("CancelledError", new CancelledException("x").Kind);
        Assert.Equal("OpenBydError", new OpenBydException("x").Kind);
    }
}
