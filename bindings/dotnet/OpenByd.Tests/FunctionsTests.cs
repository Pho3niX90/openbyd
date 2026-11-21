using System.Reflection;
using System.Text;
using System.Text.Json;
using OpenByd.Models;
using OpenByd.Testing;

namespace OpenByd.Tests;

/// <summary>What needs no client: the cipher, models by type, the runtime.</summary>
public sealed class FunctionsTests
{
    [Fact]
    public void TheVersionIsThePackages()
    {
        var package = typeof(Client).Assembly.GetCustomAttribute<AssemblyInformationalVersionAttribute>();
        // The build may add the commit after a plus sign.
        Assert.Equal(OpenBydCore.Version, package?.InformationalVersion.Split('+')[0]);
    }

    [Fact]
    public void TheCipherRoundTripsTextAndBytes()
    {
        const string text = """{"hello":"wörld"}""";
        var envelope = OpenBydCore.EncodeEnvelope(text);
        Assert.StartsWith("F", envelope);
        Assert.Equal(text, Encoding.UTF8.GetString(OpenBydCore.DecodeEnvelope(envelope)));
        byte[] bytes = [0, 255, 16, 128];
        Assert.Equal(bytes, OpenBydCore.DecodeEnvelope(OpenBydCore.EncodeEnvelope(bytes)));
        Assert.Throws<BangcleException>(() => OpenBydCore.DecodeEnvelope(""));
        Assert.ThrowsAny<BydCryptoException>(() => OpenBydCore.DecodeEnvelope("not an envelope"));
    }

    [Fact]
    public void AModelParsesByTypeTyped()
    {
        const string raw = """{"latitude": 52.0, "longitude": "4.0", "gpsTimeStamp": 1700000000}""";
        var gps = OpenBydCore.ParseModel<GpsInfo>(raw);
        Assert.Equal((52.0, 4.0), (gps.Latitude, gps.Longitude));
        Assert.Equal("2023-11-14T22:13:20Z", gps.GpsTimestamp);
        Assert.Equal(["latitude", "longitude", "gpsTimeStamp"], gps.Raw.Keys);
        Assert.Equal("4.0", gps.Raw["longitude"].GetString());

        using var document = JsonDocument.Parse("""{"raw": {"elecPercent": 80, "time": 1700000000}, "context": {"energy_type": {"EnergyType": 0}}}""");
        var realtime = OpenBydCore.ParseModel<VehicleRealtimeData>(
            document.RootElement.GetProperty("raw"), document.RootElement.GetProperty("context"));
        Assert.Equal(80, realtime.ElecPercent);
        Assert.Throws<ValidationException>(() => OpenBydCore.ParseModel<ChargingStatus>("""{"elecPercent": "lots"}"""));
        Assert.Throws<DataException>(() => OpenBydCore.ParseModel<GpsInfo>("{not json"));
    }

    [Fact]
    public void FloatsJsonCannotHoldComeBackAsNumbersOnlyWhereAFloatIs()
    {
        const string raw = """{"latitude": 52.0, "speed": "inf", "direction": "-inf", "requestSerial": "Infinity"}""";
        var gps = OpenBydCore.ParseModel<GpsInfo>(raw);
        Assert.Equal(double.PositiveInfinity, gps.Speed);
        Assert.Equal(double.NegativeInfinity, gps.Direction);
        // A string field that happens to read "Infinity" stays a string, and so does the raw reply.
        Assert.Equal("Infinity", gps.RequestSerial);
        Assert.Equal("inf", gps.Raw["speed"].GetString());
        Assert.Equal("Infinity", gps.Raw["requestSerial"].GetString());
        // pyBYD reads a NaN coordinate as no coordinate.
        Assert.Null(OpenBydCore.ParseModel<GpsInfo>("""{"latitude": "nan"}""").Latitude);

        // A model serialises to the JSON it was read from: the three strings, and 52.0 for 52.
        // (The fields follow the raw reply, which closes with a brace.)
        var json = JsonSerializer.Serialize(gps);
        Assert.Contains(
            "},\"latitude\":52.0,\"longitude\":null,\"speed\":\"Infinity\",\"direction\":\"-Infinity\",", json);
        Assert.Equal(gps.Speed, JsonSerializer.Deserialize<GpsInfo>(json)?.Speed);
        var nan = JsonSerializer.Deserialize<GpsInfo>(json.Replace("\"-Infinity\"", "\"NaN\""));
        Assert.True(double.IsNaN(nan?.Direction ?? 0));
    }

    [Fact]
    public void TheRuntimeReportsItsSettingsAndRefusesBadOnes()
    {
        Assert.True(OpenBydCore.Runtime.WorkerThreads >= 1);
        var refused = Assert.Throws<DataException>(() => OpenBydCore.ConfigureRuntime(0));
        Assert.Equal("ValueError", refused.Kind);
    }

    [Fact]
    public void AParityCallReportsPyBydsErrorClassAsData()
    {
        var answer = Parity.Call("hashing.md5_hex", """{"value": "abc"}""");
        Assert.Equal("900150983CD24FB0D6963F7D28E17F72", answer.Ok?.GetString());
        Assert.Null(answer.ErrorKind);
        var unknown = Parity.Call("no.such", "{}");
        Assert.Null(unknown.Ok);
        Assert.Equal("UnknownCall", unknown.ErrorKind);
    }
}
