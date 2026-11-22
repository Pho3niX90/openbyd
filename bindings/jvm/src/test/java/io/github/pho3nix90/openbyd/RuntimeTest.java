package io.github.pho3nix90.openbyd;

import static io.github.pho3nix90.openbyd.World.HVAC;
import static io.github.pho3nix90.openbyd.World.VIN;
import static io.github.pho3nix90.openbyd.World.await;
import static io.github.pho3nix90.openbyd.World.until;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.core.JsonProcessingException;
import io.github.pho3nix90.openbyd.model.AcSwitch;
import io.github.pho3nix90.openbyd.model.BydConfig;
import io.github.pho3nix90.openbyd.model.ClimateStartParams;
import io.github.pho3nix90.openbyd.model.EnergyType;
import io.github.pho3nix90.openbyd.model.GpsInfo;
import io.github.pho3nix90.openbyd.model.HvacFeatures;
import io.github.pho3nix90.openbyd.model.HvacStatus;
import io.github.pho3nix90.openbyd.model.PollOptions;
import io.github.pho3nix90.openbyd.model.SeatAvailability;
import io.github.pho3nix90.openbyd.model.SeatLevel;
import io.github.pho3nix90.openbyd.model.SeatPosition;
import io.github.pho3nix90.openbyd.model.SessionTokens;
import io.github.pho3nix90.openbyd.testing.Reply;
import java.lang.ref.WeakReference;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import org.junit.jupiter.api.Test;

/** The runtime's own rules: strict reading, what is sent, errors by class, and a client's life. */
class RuntimeTest {
    private static <T> T read(String json, Class<T> type) throws JsonProcessingException {
        return Json.MAPPER.readValue(json, type);
    }

    @Test
    void recordsAreReadStrictly() throws JsonProcessingException {
        SeatAvailability seat = read("{\"heat\": true, \"ventilation\": false}", SeatAvailability.class);
        assertEquals(new SeatAvailability(true, false), seat);
        for (String refused :
                List.of(
                        "{\"heat\": true, \"ventilation\": false, \"massage\": true}", // an unknown property
                        "{\"heat\": true}", // a missing property
                        "{\"heat\": true, \"ventilation\": null}", // null for a primitive
                        "{\"heat\": true, \"ventilation\": \"false\"}", // a string for a boolean
                        "{\"heat\": true, \"ventilation\": 0}")) { // a number for a boolean
            assertThrows(JsonProcessingException.class, () -> read(refused, SeatAvailability.class), refused);
        }

        String tokens = "{\"user_id\": \"1\", \"sign_token\": \"s\", \"encry_token\": \"e\","
                + " \"ttl_remaining\": %s, \"guest_vins\": %s}";
        SessionTokens session = read(tokens.formatted("null", "{\"TESTVIN0000000001\": true}"), SessionTokens.class);
        assertNull(session.ttlRemaining());
        assertEquals(Map.of(VIN, true), session.guestVins());
        // A record saved before openbyd kept the refresh token reads with none.
        assertEquals("", session.refreshToken());
        String kept = "{\"user_id\": \"1\", \"sign_token\": \"s\", \"encry_token\": \"e\","
                + " \"refresh_token\": \"r\", \"ttl_remaining\": null, \"guest_vins\": {}}";
        assertEquals("r", read(kept, SessionTokens.class).refreshToken());
        // null reads as none too, and printing never shows a token.
        assertEquals("", read(kept.replace("\"r\"", "null"), SessionTokens.class).refreshToken());
        SessionTokens secret = new SessionTokens("1", "S-secret", "E-secret", "R-secret", null, Map.of());
        for (String token : List.of("S-secret", "E-secret", "R-secret")) {
            assertFalse(secret.toString().contains(token), secret.toString());
        }
        assertTrue(secret.toString().contains("userId=1"), secret.toString());
        assertThrows(JsonProcessingException.class, () -> read(tokens.formatted("\"5\"", "{}"), SessionTokens.class));
        // Null where the type has no room for it.
        assertThrows(JsonProcessingException.class, () -> read(tokens.formatted("5.0", "null"), SessionTokens.class));
        // A number is not a string, a float is not an integer, and an enum has only its members.
        String gps = "{\"raw\": {}, \"latitude\": null, \"longitude\": null, \"speed\": null, \"direction\": null,"
                + " \"gps_timestamp\": null, \"request_serial\": %s}";
        assertEquals("S1", read(gps.formatted("\"S1\""), GpsInfo.class).requestSerial());
        assertThrows(JsonProcessingException.class, () -> read(gps.formatted("7"), GpsInfo.class));
        assertEquals(SeatLevel.HIGH, read("2", SeatLevel.class));
        assertEquals(SeatPosition.REAR_LEFT, read("\"rear_left\"", SeatPosition.class));
        for (String refused : List.of("3", "\"2\"", "2.0", "null")) {
            assertThrows(Throwable.class, () -> assertInstanceOf(SeatLevel.class, read(refused, SeatLevel.class)), refused);
        }
        assertThrows(JsonProcessingException.class, () -> read("\"boot\"", SeatPosition.class));
    }

    @Test
    void floatsJsonCannotHoldAreReadAndWrittenAsTheirStrings() throws JsonProcessingException {
        String json = "{\"available\":true,\"temp_offset\":\"NaN\",\"temp_range\":[\"-Infinity\",\"Infinity\",1.5]}";
        HvacFeatures features = read(json, HvacFeatures.class);
        assertTrue(Double.isNaN(features.tempOffset()));
        assertEquals(List.of(Double.NEGATIVE_INFINITY, Double.POSITIVE_INFINITY, 1.5), features.tempRange());
        assertEquals(json, Json.MAPPER.writeValueAsString(features));
        // Any other string is not a number.
        assertThrows(JsonProcessingException.class, () -> read(json.replace("\"NaN\"", "\"1.5\""), HvacFeatures.class));
    }

    @Test
    void whatIsNotSetIsNotSent() {
        assertEquals("{\"vin\":\"V\"}", Json.encode(Invoker.args("vin", "V"), new PollOptions()));
        assertEquals(
                "{\"vin\":\"V\",\"poll_attempts\":3,\"mqtt_timeout\":0.5}",
                Json.encode(Invoker.args("vin", "V"), new PollOptions().pollAttempts(3).mqttTimeout(0.5)));
        assertEquals(
                "{\"username\":\"user@example.com\",\"password\":\"secret\",\"control_pin\":\"123456\"}",
                Json.encode(Map.of(), new BydConfig("user@example.com", "secret").controlPin("123456")));
        // Enums cross as pyBYD's values, parameter objects under pyBYD's names, and text as ASCII.
        Map<String, Object> args =
                Invoker.args(
                        "energy_type", EnergyType.HYBRID,
                        "position", SeatPosition.REAR_LEFT,
                        "params", new ClimateStartParams().temperature(21).timeSpan(2),
                        "name", "wörld");
        assertEquals(
                "{\"energy_type\":2,\"position\":\"rear_left\",\"params\":{\"temperature\":21.0,\"time_span\":2},"
                        + "\"name\":\"w\\u00F6rld\"}",
                Json.encode(args, null));
        Map<String, Object> bad = Map.of("data", Map.of("a", List.of(1.0f, Float.NaN)));
        DataException refused = assertThrows(DataException.class, () -> Json.encode(bad, null));
        assertEquals("ValueError", refused.kind());
        assertTrue(refused.getMessage().contains("a must be a finite number, not NaN"), refused.getMessage());
    }

    private static String error(String python, String bases) {
        return "{\"class\": \"%s\", \"bases\": %s, \"message\": \"m\", \"code\": \"6024\", \"endpoint\": \"/e\","
                .formatted(python, bases)
                + " \"status_code\": 502, \"error_type\": \"int_parsing\", \"loc\": [\"a\", \"b\"]}";
    }

    @Test
    void anErrorBecomesItsClassOrTheNearestParentThisPackageKnows() {
        OpenBydException limited = Json.error(error("BydRateLimitError", "[\"BydApiError\", \"BydError\"]"));
        BydRateLimitException api = assertInstanceOf(BydRateLimitException.class, limited);
        assertEquals(
                List.of("m", "BydRateLimitError", "6024", "/e"),
                List.of(api.getMessage(), api.kind(), api.code(), api.endpoint()));

        // A class a newer core adds lands on its nearest known parent, and keeps its own name.
        String parents = "[\"BydThrottleError\", \"BydApiError\", \"BydError\"]";
        OpenBydException newer = Json.error(error("BydQuotaError", parents));
        assertEquals(BydApiException.class, newer.getClass());
        assertEquals("BydQuotaError", newer.kind());
        OpenBydException builtin = Json.error(error("OverflowError", "[\"ValueError\"]"));
        assertEquals(DataException.class, builtin.getClass());
        assertEquals("OverflowError", builtin.kind());
        assertEquals(OpenBydException.class, Json.error(error("Mystery", "[\"Riddle\"]")).getClass());
        assertEquals(OpenBydException.class, Json.error(error("ScriptExhausted", "[]")).getClass());
        assertEquals(OpenBydException.class, Json.error("not json").getClass());

        BydTransportException transport =
                assertInstanceOf(BydTransportException.class, Json.error(error("BydTransportError", "[\"BydError\"]")));
        assertEquals(502L, transport.statusCode());
        ValidationException invalid =
                assertInstanceOf(ValidationException.class, Json.error(error("ValidationError", "[\"ValueError\"]")));
        assertEquals(List.of("a", "b"), invalid.loc());
        assertEquals("int_parsing", invalid.errorType());
        assertThrows(UnsupportedOperationException.class, () -> invalid.loc().add("c"));
        // What the core leaves out has an empty value, not null.
        OpenBydException bare = Json.error("{\"class\": \"BydApiError\", \"message\": \"m\"}");
        BydApiException bareApi = assertInstanceOf(BydApiException.class, bare);
        assertEquals(List.of("", ""), List.of(bareApi.code(), bareApi.endpoint()));
        OpenBydException unanswered = Json.error("{\"class\": \"BydTransportError\"}");
        assertNull(assertInstanceOf(BydTransportException.class, unanswered).statusCode());
    }

    @Test
    void whatACallbackThrowsGoesBackToTheCoreByClass() {
        assertEquals(
                Map.of("class", "BydRateLimitError", "message", "busy", "code", "6024", "endpoint", "/e"),
                Json.errorToJson(new BydRateLimitException("busy", "6024", "/e")));
        Map<String, Object> transport = Json.errorToJson(new BydTransportException("down", 503L, "/e"));
        assertEquals(
                List.of("BydTransportError", 503L, "/e"),
                List.of(transport.get("class"), transport.get("status_code"), transport.get("endpoint")));
        assertEquals(
                Map.of("class", "TypeError", "message", "bad"),
                Json.errorToJson(new DataException("bad", "TypeError")));
        assertEquals(
                Map.of("class", "IllegalStateException", "message", ""),
                Json.errorToJson(new IllegalStateException()));
    }

    /** A client nothing refers to any more; the reference tells when it was collected. */
    private static WeakReference<Client> dropped(World world, ClientOptions options) throws Exception {
        Client client = new Client(world.config(), options);
        await(client.login());
        return new WeakReference<>(client);
    }

    private static boolean collected(WeakReference<?> reference) {
        System.gc();
        return reference.refersTo(null);
    }

    @Test
    void aDroppedClientIsClosedWhenCollectedUnlessItListensOrIsInUse() throws Exception {
        try (World world = new World()) {
            int before = Core.live();
            WeakReference<Client> quiet = dropped(world, World.options());
            WeakReference<Client> listening = dropped(world, World.options().onMqttConnect(() -> {}));
            assertEquals(before + 2, Core.live());
            until("the quiet client to be collected", () -> collected(quiet));
            until("its native client to be freed", () -> Core.live() == before + 1);
            // The listening one waits for pushes: only closing it ends that.
            assertFalse(listening.refersTo(null));
            Client kept = listening.get();
            assertEquals(AcSwitch.OFF, await(kept.getHvacStatus(VIN)).acSwitch());
            kept.close();
            assertEquals(before, Core.live());

            // A call in flight keeps its client, and so its callbacks, until it ends.
            world.server.reply(HVAC, Reply.data(Map.of("acSwitch", 2)).delay(1));
            CompletableFuture<HvacStatus> inFlight = new Client(world.config(), World.options()).getHvacStatus(VIN);
            for (int round = 0; round < 20 && !inFlight.isDone(); round++) {
                System.gc();
                World.sleep(50);
            }
            assertEquals(AcSwitch.ON, await(inFlight).acSwitch());
            until("the idle client to be freed", () -> {
                System.gc();
                return Core.live() == before;
            });
        }
    }
}
