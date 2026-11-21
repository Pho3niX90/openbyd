package io.github.pho3nix90.openbyd;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.pho3nix90.openbyd.model.ChargingStatus;
import io.github.pho3nix90.openbyd.model.GpsInfo;
import io.github.pho3nix90.openbyd.model.PollOptions;
import io.github.pho3nix90.openbyd.model.VehicleRealtimeData;
import io.github.pho3nix90.openbyd.testing.Parity;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;
import org.junit.jupiter.api.Test;

/** What needs no client: the cipher, models by class, the runtime. */
class FunctionsTest {
    @Test
    void theVersionIsThePackages() {
        assertEquals(System.getProperty("openbyd.version"), OpenByd.version());
    }

    @Test
    void theCipherRoundTripsTextAndBytes() {
        String text = "{\"hello\":\"wörld\"}";
        String envelope = OpenByd.encodeEnvelope(text);
        assertTrue(envelope.startsWith("F"), envelope);
        assertEquals(text, new String(OpenByd.decodeEnvelope(envelope), StandardCharsets.UTF_8));
        byte[] bytes = {0, (byte) 255, 16, (byte) 128};
        assertArrayEquals(bytes, OpenByd.decodeEnvelope(OpenByd.encodeEnvelope(bytes)));
        BangcleException empty = assertThrows(BangcleException.class, () -> OpenByd.decodeEnvelope(""));
        assertEquals("BangcleError", empty.kind());
        assertThrows(BydCryptoException.class, () -> OpenByd.decodeEnvelope("not an envelope"));
    }

    @Test
    void aModelParsesByClassTyped() {
        Map<String, Object> raw = new LinkedHashMap<>();
        raw.put("latitude", 52.0);
        raw.put("longitude", "4.0");
        raw.put("gpsTimeStamp", 1_700_000_000);
        GpsInfo gps = OpenByd.parseModel(GpsInfo.class, raw);
        assertEquals(52.0, gps.latitude());
        assertEquals(4.0, gps.longitude());
        assertNotNull(gps.gpsTimestamp());
        assertNull(gps.speed());
        assertEquals(raw, gps.raw());

        Map<String, Object> context = Map.of("energy_type", Map.of("EnergyType", 0));
        VehicleRealtimeData realtime =
                OpenByd.parseModel(
                        VehicleRealtimeData.class, Map.of("elecPercent", 80, "time", 1_700_000_000), context);
        assertEquals(80.0, realtime.elecPercent());
        ValidationException invalid =
                assertThrows(
                        ValidationException.class,
                        () -> OpenByd.parseModel(ChargingStatus.class, Map.of("elecPercent", "lots")));
        assertEquals("ValidationError", invalid.kind());
        assertThrows(IllegalArgumentException.class, () -> OpenByd.parseModel(PollOptions.class, Map.of()));
        assertThrows(IllegalArgumentException.class, () -> OpenByd.parseModel(RuntimeInfo.class, Map.of()));
    }

    @Test
    void floatsJsonCannotHoldComeBackAsNumbersOnlyWhereAFloatIs() {
        Map<String, Object> raw = new LinkedHashMap<>();
        raw.put("latitude", 52.0);
        raw.put("speed", "inf");
        raw.put("direction", "-inf");
        raw.put("requestSerial", "Infinity");
        GpsInfo gps = OpenByd.parseModel(GpsInfo.class, raw);
        assertEquals(Double.POSITIVE_INFINITY, gps.speed());
        assertEquals(Double.NEGATIVE_INFINITY, gps.direction());
        // A string field that happens to read "Infinity" stays a string, and so does the raw reply.
        assertEquals("Infinity", gps.requestSerial());
        assertEquals(raw, gps.raw());
        assertEquals("inf", gps.raw().get("speed"));
        // pyBYD reads a NaN coordinate as no coordinate.
        assertNull(OpenByd.parseModel(GpsInfo.class, Map.of("latitude", "nan")).latitude());
    }

    @Test
    void theRuntimeReportsItsSettingsAndRefusesBadOnes() {
        RuntimeInfo info = OpenByd.runtimeInfo();
        assertTrue(info.workerThreads() >= 1);
        DataException refused = assertThrows(DataException.class, () -> OpenByd.configureRuntime(0));
        assertEquals("ValueError", refused.kind());
    }

    @Test
    void aParityCallReportsPybydsErrorClassAsData() {
        assertEquals(
                "{\"ok\":\"900150983CD24FB0D6963F7D28E17F72\"}", Parity.call("hashing.md5_hex", "{\"value\": \"abc\"}"));
        String unknown = Parity.call("no.such", "{}");
        assertTrue(unknown.startsWith("{\"error\":{\"kind\":\"UnknownCall\""), unknown);
    }
}
