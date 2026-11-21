// Checks a packed jar outside its build: the C library for this platform
// loads from the jar, and a client reaches the network layer. The release
// workflow runs it on every platform before publishing.
//
//   java -cp "openbyd-<version>.jar:deps/*" Smoke.java <version>

import io.github.pho3nix90.openbyd.BydTransportException;
import io.github.pho3nix90.openbyd.Client;
import io.github.pho3nix90.openbyd.OpenByd;
import io.github.pho3nix90.openbyd.model.BydConfig;
import io.github.pho3nix90.openbyd.model.GpsInfo;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import java.util.concurrent.ExecutionException;

public class Smoke {
    public static void main(String[] args) throws Exception {
        String expected = args[0];
        check(OpenByd.version().equals(expected), "the library is this release's, not " + OpenByd.version());

        String envelope = OpenByd.encodeEnvelope("{\"hello\":\"world\"}");
        String plain = new String(OpenByd.decodeEnvelope(envelope), StandardCharsets.UTF_8);
        check(plain.equals("{\"hello\":\"world\"}"), "the cipher round-trips");
        GpsInfo gps = OpenByd.parseModel(GpsInfo.class, Map.of("latitude", 52.0, "longitude", 4.0));
        check(Double.valueOf(52.0).equals(gps.latitude()), "a model parses");

        // Nothing listens on port 9: the request is built, enveloped and sent, and the connection is refused.
        BydConfig config = new BydConfig("user@example.com", "secret").baseUrl("http://127.0.0.1:9");
        try (Client client = new Client(config)) {
            try {
                client.login().get();
                check(false, "the login cannot succeed");
            } catch (ExecutionException failure) {
                check(failure.getCause() instanceof BydTransportException, "a transport error, not " + failure.getCause());
            }
        }
        System.out.println(
                "openbyd " + expected + " works on " + System.getProperty("os.name") + " "
                        + System.getProperty("os.arch") + " (Java " + System.getProperty("java.version") + ")");
    }

    private static void check(boolean holds, String what) {
        if (!holds) {
            throw new AssertionError(what);
        }
    }
}
