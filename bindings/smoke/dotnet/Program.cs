// Checks a packed NuGet package in a project of its own: the C library for
// this platform comes with the package, and a client reaches the network
// layer. The release workflow runs it on every platform before publishing.
//
//   dotnet run --project bindings/smoke/dotnet -p:OpenBydVersion=<version> -- <version>

using System.Text;
using System.Text.Json;
using OpenByd;
using OpenByd.Models;

var expected = args[0];
Check(OpenBydCore.Version == expected, $"the library is this release's, not {OpenBydCore.Version}");

var envelope = OpenBydCore.EncodeEnvelope("{\"hello\":\"world\"}");
Check(Encoding.UTF8.GetString(OpenBydCore.DecodeEnvelope(envelope)) == "{\"hello\":\"world\"}", "the cipher round-trips");
using var raw = JsonDocument.Parse("{\"latitude\": 52.0, \"longitude\": 4.0}");
Check(OpenBydCore.ParseModel<GpsInfo>(raw.RootElement).Latitude == 52.0, "a model parses");

// Nothing listens on port 9: the request is built, enveloped and sent, and the connection is refused.
using (var client = new Client(new BydConfig { Username = "user@example.com", Password = "secret", BaseUrl = "http://127.0.0.1:9" }))
{
    try
    {
        await client.LoginAsync();
        Check(false, "the login cannot succeed");
    }
    catch (BydTransportException)
    {
    }
}

Console.WriteLine($"OpenByd {expected} works on {System.Runtime.InteropServices.RuntimeInformation.RuntimeIdentifier} (.NET {Environment.Version})");

static void Check(bool holds, string what)
{
    if (!holds)
    {
        throw new InvalidOperationException(what);
    }
}
