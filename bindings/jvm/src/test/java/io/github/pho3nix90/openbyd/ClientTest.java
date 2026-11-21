package io.github.pho3nix90.openbyd;

import static io.github.pho3nix90.openbyd.World.HOMEPAGE;
import static io.github.pho3nix90.openbyd.World.HVAC;
import static io.github.pho3nix90.openbyd.World.LATEST_CONFIG;
import static io.github.pho3nix90.openbyd.World.LOGIN;
import static io.github.pho3nix90.openbyd.World.PASSWORD;
import static io.github.pho3nix90.openbyd.World.PIN;
import static io.github.pho3nix90.openbyd.World.REALTIME;
import static io.github.pho3nix90.openbyd.World.REMOTE;
import static io.github.pho3nix90.openbyd.World.USER;
import static io.github.pho3nix90.openbyd.World.VEHICLES;
import static io.github.pho3nix90.openbyd.World.VIN;
import static io.github.pho3nix90.openbyd.World.await;
import static io.github.pho3nix90.openbyd.World.functions;
import static io.github.pho3nix90.openbyd.World.thrown;
import static io.github.pho3nix90.openbyd.World.until;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import io.github.pho3nix90.openbyd.model.AcSwitch;
import io.github.pho3nix90.openbyd.model.BydConfig;
import io.github.pho3nix90.openbyd.model.CapabilitiesOptions;
import io.github.pho3nix90.openbyd.model.ChargingHomepage;
import io.github.pho3nix90.openbyd.model.CommandAckDiagnostics;
import io.github.pho3nix90.openbyd.model.CommandGateVerdict;
import io.github.pho3nix90.openbyd.model.CommandOptions;
import io.github.pho3nix90.openbyd.model.DeviceProfile;
import io.github.pho3nix90.openbyd.model.EnergyType;
import io.github.pho3nix90.openbyd.model.HvacStatus;
import io.github.pho3nix90.openbyd.model.NotificationCategory;
import io.github.pho3nix90.openbyd.model.NotificationOptions;
import io.github.pho3nix90.openbyd.model.PollOptions;
import io.github.pho3nix90.openbyd.model.RemoteControlResult;
import io.github.pho3nix90.openbyd.model.RequestRecord;
import io.github.pho3nix90.openbyd.model.RunStandbyPlanOptions;
import io.github.pho3nix90.openbyd.model.SeatClimateParams;
import io.github.pho3nix90.openbyd.model.SeedOptions;
import io.github.pho3nix90.openbyd.model.SessionInfo;
import io.github.pho3nix90.openbyd.model.SessionTokens;
import io.github.pho3nix90.openbyd.model.StandbyPlan;
import io.github.pho3nix90.openbyd.model.StandbyPlanList;
import io.github.pho3nix90.openbyd.model.StandbyPlanRunResult;
import io.github.pho3nix90.openbyd.model.StandbySchedule;
import io.github.pho3nix90.openbyd.model.StandbyScheduleOptions;
import io.github.pho3nix90.openbyd.model.StandbyScheduleRepeat;
import io.github.pho3nix90.openbyd.model.Vehicle;
import io.github.pho3nix90.openbyd.model.VehicleLatestConfig;
import io.github.pho3nix90.openbyd.model.VehicleMsgItemInfo;
import io.github.pho3nix90.openbyd.model.VehicleRealtimeData;
import io.github.pho3nix90.openbyd.testing.FakeServer;
import io.github.pho3nix90.openbyd.testing.RecordedRequest;
import io.github.pho3nix90.openbyd.testing.Reply;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import org.junit.jupiter.api.Test;

/** The client end to end against the fake server: reads, failures, every option, sessions. */
class ClientTest {
    private static final Map<String, Object> DONE = Map.of("controlState", 1, "requestSerial", "S1");

    @Test
    void theLibraryUnderTestHasTheFakeServer() {
        assertTrue(FakeServer.isAvailable(), "build it with: cargo build -p openbyd-capi --features testing");
    }

    @Test
    void readsReturnPybydsDumpsTyped() throws Exception {
        try (World world = new World()) {
            BydConfig config =
                    world.config().countryCode("DE").language("de").device(new DeviceProfile().model("Pixel"));
            Client client = world.client(config);
            assertNull(client.exportSession());
            assertFalse(client.commandsEnabled());
            assertFalse(client.mqttIsRunning());
            await(client.login());

            List<Vehicle> vehicles = await(client.getVehicles());
            assertEquals(1, vehicles.size());
            Vehicle vehicle = vehicles.get(0);
            assertEquals(VIN, vehicle.vin());
            assertEquals(EnergyType.EV, vehicle.energyType());
            assertEquals("SEAL", vehicle.modelName());
            assertEquals(Map.of("vin", VIN, "energyType", "0", "modelName", "SEAL"), vehicle.raw());

            RecordedRequest login = world.server.requests(LOGIN).get(0);
            Map<String, Object> outer = login.outer();
            assertNotNull(outer);
            assertEquals(
                    List.of("DE", "de", "Pixel"),
                    Arrays.asList(outer.get("countryCode"), outer.get("language"), outer.get("model")));

            HvacStatus hvac = await(client.getHvacStatus(VIN));
            assertEquals(AcSwitch.OFF, hvac.acSwitch());
            assertEquals(21.0, hvac.tempInCar());
            assertEquals(true, await(client.getVehicleCapabilities(VIN)).lock());
            CapabilitiesOptions refresh = new CapabilitiesOptions().forceRefresh(true);
            assertEquals(VIN, await(client.getVehicleCapabilities(VIN, refresh)).vin());
            Map<String, VehicleLatestConfig> configs = await(client.getLatestConfigs(List.of(VIN)));
            assertEquals(Set.of(VIN), configs.keySet());
            assertNotNull(await(client.getLatestConfig(VIN)));

            world.server.reply(HOMEPAGE, Reply.data(Map.of("elecPercent", 80, "chargingState", 0)));
            ChargingHomepage homepage = await(client.getChargingHomepage(VIN));
            assertEquals(80L, homepage.status().soc());
            assertNotNull(homepage.schedule().vin());

            SessionInfo session = await(client.ensureSession());
            assertFalse(session.signToken().isEmpty());
            assertTrue(session.ttl() > 0);
            Vehicle known = client.vehicleMetadata(VIN);
            assertNotNull(known);
            assertEquals("SEAL", known.modelName());
            assertNull(client.vehicleMetadata("TESTVIN0000000009"));
            CommandAckDiagnostics diagnostics = client.getCommandAckDiagnostics();
            assertEquals(List.of(0L, 0L), List.of(diagnostics.pending(), diagnostics.matched()));
        }
    }

    @Test
    void failuresArePybydsExceptionClassesWithTheirFields() {
        try (World world = new World()) {
            Client client = world.client();
            world.server.reply(HOMEPAGE, Reply.code("1001"));
            Throwable failed = thrown(() -> client.getChargingStatus(VIN));
            BydEndpointNotSupportedException unsupported =
                    assertInstanceOf(BydEndpointNotSupportedException.class, failed);
            assertInstanceOf(BydApiException.class, failed);
            assertInstanceOf(BydException.class, failed);
            assertInstanceOf(OpenBydException.class, failed);
            assertInstanceOf(RuntimeException.class, failed);
            assertEquals("BydEndpointNotSupportedError", unsupported.kind());
            assertEquals("1001", unsupported.code());
            assertEquals(HOMEPAGE, unsupported.endpoint());

            world.server.reply(HVAC, Reply.status(503, "down"));
            BydTransportException transport =
                    assertInstanceOf(BydTransportException.class, thrown(() -> client.getHvacStatus(VIN)));
            assertEquals(503L, transport.statusCode());
            assertEquals(HVAC, transport.endpoint());
            assertEquals("BydTransportError", transport.kind());

            // What the types cannot rule out is refused by pyBYD's own validation.
            ValidationException invalid =
                    assertInstanceOf(
                            ValidationException.class,
                            thrown(() -> client.setSeatClimate(VIN, new SeatClimateParams().mainHeat(9))));
            assertInstanceOf(DataException.class, invalid);
            assertEquals(List.of("main_heat"), invalid.loc());
            assertFalse(invalid.errorType().isEmpty());
            assertEquals("ValidationError", invalid.kind());
        }
    }

    @Test
    void aNumberJsonCannotHoldIsRefusedNotSent() {
        try (World world = new World()) {
            Client client = world.client();
            for (double bad : new double[] {Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY}) {
                // A method that waits on the network fails its future.
                CompletableFuture<VehicleRealtimeData> refused =
                        client.getVehicleRealtime(VIN, new PollOptions().pollInterval(bad));
                assertTrue(refused.isCompletedExceptionally());
                DataException data = assertInstanceOf(DataException.class, thrown(() -> refused));
                assertFalse(data instanceof ValidationException);
                assertEquals("ValueError", data.kind());
                assertTrue(data.getMessage().contains("poll_interval"), data.getMessage());

                // Also deep inside an argument.
                Map<String, Object> vehicle = Map.of("vin", VIN, "mileage", List.of(1.5, bad));
                assertInstanceOf(
                        DataException.class, thrown(() -> client.getCar(VIN, new GetCarOptions().vehicle(vehicle))));
            }
            assertEquals(List.of(), world.server.requests(), "nothing was sent");
        }
    }

    @Test
    void aBadConfigIsRefusedWhenTheClientIsMade() {
        BydConfig config = new BydConfig(USER, PASSWORD);
        DataException retries =
                assertThrows(DataException.class, () -> new Client(config, new ClientOptions().commandRetries(0)));
        assertEquals("ValueError", retries.kind());
        assertThrows(
                DataException.class,
                () -> new Client(config, new ClientOptions().commandAckTtlSeconds(Double.POSITIVE_INFINITY)));
        assertThrows(DataException.class, () -> new Client(config.sessionTtl(Double.NaN)));
        ClientOptions spelled = new ClientOptions().gatePolicy(GatePolicy.STRICT).relogin(ReloginPolicy.PASSWORD);
        new Client(new BydConfig(USER, PASSWORD), spelled).close();
    }

    @Test
    void aClientFailsAfterClose() throws Exception {
        try (World world = new World()) {
            Client client = world.client();
            assertFalse(client.closed());
            assertEquals(AcSwitch.OFF, await(client.getHvacStatus(VIN)).acSwitch());
            client.close();
            assertTrue(client.closed());
            BydException closed = assertInstanceOf(BydException.class, thrown(() -> client.getHvacStatus(VIN)));
            assertTrue(closed.getMessage().contains("closed"), closed.getMessage());
            assertEquals("BydError", closed.kind());
            assertThrows(BydException.class, client::exportSession);
            client.close();
            try (Client scoped = new Client(world.config(), World.options())) {
                await(scoped.login());
                assertFalse(scoped.closed());
            }
        }
    }

    @Test
    void closingFailsTheCallsInFlightAndNothingHangs() throws Exception {
        try (World world = new World()) {
            world.server.reply(HVAC, Reply.data(Map.of("acSwitch", 1)).delay(3));
            Client client = world.client();
            await(client.login());
            List<CompletableFuture<HvacStatus>> waiting = new ArrayList<>();
            for (int index = 0; index < 5; index++) {
                waiting.add(client.getHvacStatus(VIN));
            }
            until("the requests", () -> world.server.requests(HVAC).size() == 5);
            long started = System.nanoTime();
            client.close();
            for (CompletableFuture<HvacStatus> call : waiting) {
                CancelledException cancelled = assertInstanceOf(CancelledException.class, thrown(() -> call));
                assertEquals("CancelledError", cancelled.kind());
            }
            assertTrue(System.nanoTime() - started < 2_000_000_000L, "closing did not wait for the server");
        }
    }

    private static Object lockWith(World world, ClientOptions options, boolean missing) throws Exception {
        world.server.reply(LATEST_CONFIG, Reply.data(Map.of(VIN, missing ? functions() : functions("1005"))));
        world.server.reply(REMOTE, Reply.data(DONE));
        Client client = world.client(options);
        await(client.verifyCommandAccess(VIN));
        assertTrue(client.commandsEnabled());
        try {
            return await(client.lock(VIN));
        } catch (OpenBydException error) {
            return error;
        }
    }

    @Test
    void theGatePolicyDecidesWhetherACommandIsSent() throws Exception {
        try (World world = new World()) {
            BydEndpointNotSupportedException refused =
                    assertInstanceOf(BydEndpointNotSupportedException.class, lockWith(world, new ClientOptions(), true));
            assertEquals("command_gate_blocked", refused.code());
            assertEquals(List.of(), world.server.requests(REMOTE));

            ClientOptions lenient = new ClientOptions().gatePolicy(GatePolicy.ALLOW_MISSING_DATA);
            assertTrue(assertInstanceOf(RemoteControlResult.class, lockWith(world, lenient, true)).success());

            List<Map.Entry<String, CommandGateVerdict>> verdicts = new CopyOnWriteArrayList<>();
            ClientOptions deciding =
                    new ClientOptions()
                            .gatePolicy(
                                    (vin, verdict) -> {
                                        verdicts.add(Map.entry(vin, verdict));
                                        return verdict.reason().equals("function_no_missing");
                                    });
            assertTrue(assertInstanceOf(RemoteControlResult.class, lockWith(world, deciding, true)).success());
            assertEquals(VIN, verdicts.get(0).getKey());
            assertFalse(verdicts.get(0).getValue().supported());

            // A policy that throws falls back to the verdict, and the failure is reported.
            List<Map.Entry<String, Throwable>> reported = new CopyOnWriteArrayList<>();
            ClientOptions broken =
                    new ClientOptions()
                            .gatePolicy(
                                    (vin, verdict) -> {
                                        throw new IllegalStateException("policy bug");
                                    })
                            .onCallbackError((callback, error) -> reported.add(Map.entry(callback, error)));
            assertInstanceOf(BydEndpointNotSupportedException.class, lockWith(world, broken, true));
            assertEquals("gatePolicy", reported.get(0).getKey());
            assertTrue(reported.get(0).getValue().toString().contains("policy bug"));
        }
    }

    private static Object commandPwd(RecordedRequest request) {
        return assertInstanceOf(Map.class, request.inner()).get("commandPwd");
    }

    @Test
    void thePinResolverReplacesPybydsRule() throws Exception {
        try (World world = new World()) {
            String digest = "0123456789abcdef0123456789abcdef";
            List<List<String>> calls = new CopyOnWriteArrayList<>();
            ClientOptions resolving =
                    new ClientOptions()
                            .pinResolver(
                                    (commandPwd, controlPin) -> {
                                        calls.add(Arrays.asList(commandPwd, controlPin));
                                        return digest;
                                    });
            lockWith(world, resolving, false);
            assertEquals(Arrays.asList(null, PIN), calls.get(0));
            assertEquals(digest, commandPwd(world.server.requests(REMOTE).get(0)));

            // An answer that is no string falls back to pyBYD's rule: MD5, upper case.
            world.server.clearRequests();
            lockWith(world, new ClientOptions().pinResolver((commandPwd, controlPin) -> noString()), false);
            assertEquals("E10ADC3949BA59ABBE56E057F20F883E", commandPwd(world.server.requests(REMOTE).get(0)));

            // So does a resolver that throws, and that is reported.
            world.server.clearRequests();
            List<String> reported = new CopyOnWriteArrayList<>();
            ClientOptions broken =
                    new ClientOptions()
                            .pinResolver(
                                    (commandPwd, controlPin) -> {
                                        throw new IllegalStateException("resolver bug");
                                    })
                            .onCallbackError((callback, error) -> reported.add(callback));
            lockWith(world, broken, false);
            assertEquals("E10ADC3949BA59ABBE56E057F20F883E", commandPwd(world.server.requests(REMOTE).get(0)));
            // Asked for the PIN check and for the command.
            until("the reports", () -> reported.size() == 2);
            assertEquals(List.of("pinResolver", "pinResolver"), reported);
        }
    }

    /** What a careless resolver returns: Java's types cannot stop it. */
    private static String noString() {
        return null;
    }

    @Test
    void aCommandIsRetriedAsManyTimesAsAsked() throws Exception {
        try (World world = new World()) {
            world.server.reply(LATEST_CONFIG, Reply.data(Map.of(VIN, functions("1005"))));
            world.server.reply(REMOTE, Reply.data(Map.of("controlState", 2, "requestSerial", "S1")));
            Client client = world.client(new ClientOptions().commandRetries(1));
            await(client.verifyCommandAccess(VIN));
            BydApiException failed = assertInstanceOf(BydApiException.class, thrown(() -> client.lock(VIN)));
            assertEquals("2", failed.code());
            assertEquals(1, world.server.requests(REMOTE).size(), "one attempt, no retry");

            world.server.script(REMOTE, Reply.code("5005"));
            CommandOptions pin = new CommandOptions().commandPwd("654321");
            assertInstanceOf(BydControlPasswordException.class, thrown(() -> client.lock(VIN, pin)));
        }
    }

    @Test
    void theRequestObserverHearsEveryRequest() throws Exception {
        try (World world = new World()) {
            List<RequestRecord> records = new CopyOnWriteArrayList<>();
            world.server.reply(HOMEPAGE, Reply.code("1001"));
            Client client = world.client(new ClientOptions().requestObserver(records::add));
            await(client.login());
            await(client.getHvacStatus(VIN));
            thrown(() -> client.getChargingStatus(VIN));
            until("three records", () -> records.size() == 3);
            assertEquals(
                    List.of(List.of(LOGIN, "reply", "0"), List.of(HVAC, "reply", "0"), List.of(HOMEPAGE, "reply", "1001")),
                    records.stream()
                            .map(record -> Arrays.asList(record.endpoint(), record.outcome(), record.code()))
                            .toList());
            assertTrue(records.stream().allMatch(record -> record.duration() >= 0));
            assertTrue(records.stream().allMatch(record -> record.status() == null && record.error() == null));

            // An observer that throws is reported and does not affect the client.
            List<String> reported = new CopyOnWriteArrayList<>();
            ClientOptions noisy =
                    new ClientOptions()
                            .requestObserver(
                                    record -> {
                                        throw new IllegalStateException("observer bug");
                                    })
                            .onCallbackError((callback, error) -> reported.add(callback));
            assertEquals(AcSwitch.OFF, await(world.client(noisy).getHvacStatus(VIN)).acSwitch());
            until("the report", () -> reported.contains("requestObserver"));
        }
    }

    @Test
    void aCallbacksFailureIsLoggedAsAWarningByDefault() throws Exception {
        List<LogRecord> logged = new CopyOnWriteArrayList<>();
        Handler handler =
                new Handler() {
                    @Override
                    public void publish(LogRecord record) {
                        logged.add(record);
                    }

                    @Override
                    public void flush() {}

                    @Override
                    public void close() {}
                };
        Logger logger = Logger.getLogger("io.github.pho3nix90.openbyd");
        logger.addHandler(handler);
        logger.setUseParentHandlers(false);
        try (World world = new World()) {
            ClientOptions noisy =
                    new ClientOptions()
                            .requestObserver(
                                    record -> {
                                        throw new IllegalStateException("observer bug");
                                    });
            await(world.client(noisy).login());
            until("the warning", () -> !logged.isEmpty());
        } finally {
            logger.removeHandler(handler);
            logger.setUseParentHandlers(true);
        }
        LogRecord warning = logged.get(0);
        assertEquals(Level.WARNING, warning.getLevel());
        assertTrue(warning.getMessage().matches(".*requestObserver.*observer bug.*"), warning.getMessage());
        assertInstanceOf(IllegalStateException.class, warning.getThrown());
    }

    @Test
    void sessionsAreExportedRestoredAndRenewed() throws Exception {
        try (World world = new World()) {
            Client first = world.client();
            await(first.login());
            SessionTokens saved = world.session(first);
            Double left = saved.ttlRemaining();
            assertTrue(left != null && left > 0);
            assertFalse(saved.userId().isEmpty() || saved.signToken().isEmpty() || saved.encryToken().isEmpty());
            assertEquals(Map.of(), saved.guestVins());

            // Without a password the restored client never logs in by itself.
            BydConfig withoutPassword = world.config("");
            Client restored = await(Client.fromSession(saved, withoutPassword, World.options()));
            try {
                SessionTokens resumed = restored.exportSession();
                assertNotNull(resumed);
                assertEquals(saved.signToken(), resumed.signToken());
                assertEquals(AcSwitch.OFF, await(restored.getHvacStatus(VIN)).acSwitch());
                assertEquals(1, world.server.requests(LOGIN).size(), "the restored client never logged in");

                world.server.expireSessions(USER);
                BydSessionExpiredException expired =
                        assertInstanceOf(BydSessionExpiredException.class, thrown(() -> restored.getHvacStatus(VIN)));
                assertEquals("1002", expired.code(), "BYD's own expiry code, not relogin_disabled");
                restored.invalidateSession();
            } finally {
                restored.close();
            }
            SessionTokens nameless =
                    new SessionTokens("", saved.signToken(), saved.encryToken(), saved.ttlRemaining(), saved.guestVins());
            Throwable refusedSession = thrown(() -> Client.fromSession(nameless, withoutPassword, World.options()));
            assertInstanceOf(DataException.class, refusedSession);
            // With a password it logs in again when the session expires, as any client.
            Client keyed = world.keep(await(Client.fromSession(saved, world.config(), World.options())));
            assertEquals(AcSwitch.OFF, await(keyed.getHvacStatus(VIN)).acSwitch());

            // A relogin function hands over a new session when one is needed.
            AtomicInteger asked = new AtomicInteger();
            ReloginPolicy renewing =
                    () -> {
                        asked.incrementAndGet();
                        Client helper = world.client();
                        return helper.login().thenApply(nothing -> world.session(helper));
                    };
            assertEquals(
                    AcSwitch.OFF,
                    await(world.client(new ClientOptions().relogin(renewing)).getHvacStatus(VIN)).acSwitch());
            assertEquals(1, asked.get());

            // So does one that waits for the session itself.
            ReloginPolicy waiting =
                    ReloginPolicy.blocking(
                            () -> {
                                Client helper = world.client();
                                helper.login().join();
                                return world.session(helper);
                            });
            assertEquals(
                    AcSwitch.OFF,
                    await(world.client(new ClientOptions().relogin(waiting)).getHvacStatus(VIN)).acSwitch());

            // What it throws is what the call throws.
            ReloginPolicy refusing =
                    () -> {
                        throw new BydAuthenticationException("no", "1002", "x");
                    };
            Client refused = world.client(new ClientOptions().relogin(refusing));
            BydAuthenticationException authentication =
                    assertInstanceOf(BydAuthenticationException.class, thrown(() -> refused.getHvacStatus(VIN)));
            assertEquals(List.of("no", "1002"), List.of(authentication.getMessage(), authentication.code()));

            // And so is what its stage fails with.
            ReloginPolicy failing =
                    () -> CompletableFuture.failedFuture(new BydTransportException("down", 502L, "/relogin"));
            Client failed = world.client(new ClientOptions().relogin(failing));
            BydTransportException transport =
                    assertInstanceOf(BydTransportException.class, thrown(() -> failed.getHvacStatus(VIN)));
            assertEquals(502L, transport.statusCode());

            ReloginPolicy broken =
                    () -> {
                        throw new IllegalStateException("bug");
                    };
            Client buggy = world.client(new ClientOptions().relogin(broken));
            BydException failure = assertInstanceOf(BydException.class, thrown(() -> buggy.getHvacStatus(VIN)));
            assertTrue(failure.getMessage().contains("IllegalStateException: bug"), failure.getMessage());

            Client nonsense = world.client(new ClientOptions().relogin(() -> noSession()));
            String complaint = thrown(() -> nonsense.getHvacStatus(VIN)).toString();
            assertTrue(complaint.contains("relogin returned no session"), complaint);
        }
    }

    /** What a careless relogin function returns: Java's types cannot stop it. */
    private static CompletableFuture<SessionTokens> noSession() {
        return null;
    }

    @Test
    void seededMetadataSavesTheVehicleList() throws Exception {
        try (World world = new World()) {
            world.server.reply(REALTIME, Reply.data(Map.of("elecPercent", 50, "time", 1_700_000_000)));
            Client client = world.client();
            client.seedVehicleMetadata(VIN, EnergyType.HYBRID, new SeedOptions().modelName("SONG"));
            Vehicle seeded = client.vehicleMetadata(VIN);
            assertNotNull(seeded);
            assertEquals(EnergyType.HYBRID, seeded.energyType());
            assertEquals("SONG", seeded.modelName());
            PollOptions once = new PollOptions().pollAttempts(0).mqttTimeout(0);
            assertEquals(50.0, await(client.getVehicleRealtime(VIN, once)).elecPercent());
            assertEquals(List.of(), world.server.requests(VEHICLES));

            client.seedVehicleMetadata("TESTVIN0000000002", EnergyType.EV);
            Vehicle bare = client.vehicleMetadata("TESTVIN0000000002");
            assertNotNull(bare);
            assertEquals(EnergyType.EV, bare.energyType());

            NotificationOptions system = new NotificationOptions().category(NotificationCategory.SYSTEM);
            List<VehicleMsgItemInfo> messages;
            try {
                messages = await(client.getNotifications(system));
            } catch (OpenBydException unanswered) {
                messages = List.of();
            }
            assertEquals(List.of(), messages);
        }
    }

    @Test
    void manyClientsAreCheapAndRunAtOnce() throws Exception {
        try (World world = new World()) {
            world.server.reply(HVAC, Reply.data(Map.of("acSwitch", 1)).delay(0.02));
            List<CompletableFuture<HvacStatus>> statuses = new ArrayList<>();
            for (int index = 0; index < 100; index++) {
                statuses.add(world.client().getHvacStatus(VIN));
            }
            for (CompletableFuture<HvacStatus> status : statuses) {
                assertEquals(AcSwitch.OFF, await(status).acSwitch());
            }
            assertEquals(100, world.server.requests(LOGIN).size());
        }
    }

    @Test
    void standbyPlansRoundTripWithZone() throws Exception {
        String listEndpoint = "/control/standby-vehicles/list";
        String saveEndpoint = "/control/standby-vehicles/save";
        String setDefaultEndpoint = "/control/standby-vehicles/setDefault";
        String deleteEndpoint = "/control/standby-vehicles/delete";
        String sendEndpoint = "/control/standby-vehicles/send";
        String sendResultEndpoint = "/control/standby-vehicles/send-result";

        try (World world = new World()) {
            List<Map<String, Object>> commands =
                    List.of(
                            Map.of(
                                    "commandType", "OPENAIR",
                                    "commandId", "1",
                                    "controlParamsMap", "{\"temperature\":21,\"timeSpan\":2}"));
            Map<String, Object> preset =
                    Map.of(
                            "id", "101",
                            "planName", "Preset",
                            "planType", 1,
                            "defaultPlan", 1,
                            "enable", 1,
                            "executeType", "2",
                            "startTime", "",
                            "weeklyDays", List.of(),
                            "controlParams", commands);
            Map<String, Object> created =
                    Map.of(
                            "id", "202",
                            "planName", "Morning",
                            "planType", 2,
                            "defaultPlan", 0,
                            "enable", 1,
                            "executeType", "1",
                            "startTime", "",
                            "weeklyDays", List.of(),
                            "controlParams", commands);
            world.server.reply(saveEndpoint, Reply.data(Map.of()));
            world.server.reply(setDefaultEndpoint, Reply.data(Map.of()));
            world.server.reply(deleteEndpoint, Reply.data(Map.of()));
            world.server.reply(sendEndpoint, Reply.data(Map.of("requestSerial", "RUN1")));
            world.server.reply(
                    sendResultEndpoint, Reply.data(Map.of("requestSerial", "RUN1", "res", 2, "message", "ok")));
            world.server.script(
                    listEndpoint,
                    Reply.data(List.of(preset)),
                    Reply.data(List.of(preset)),
                    Reply.data(List.of(preset)),
                    Reply.data(List.of(preset, created)),
                    Reply.data(List.of(preset, created)),
                    Reply.data(List.of(preset)));

            Client client = world.client();
            StandbyPlanList list = await(client.getStandbyPlans(VIN));
            assertEquals(1, list.plans().size());
            StandbyPlan plan = list.plans().get(0);

            StandbySchedule daily =
                    new StandbySchedule(StandbyScheduleRepeat.DAILY)
                            .at("2026-10-24T08:00:00+02:00")
                            .zone("Europe/Madrid");
            StandbyPlanList saved =
                    await(client.saveStandbyPlan(VIN, plan, new StandbyScheduleOptions().schedule(daily)));
            assertEquals("101", saved.plans().get(0).id());
            Map<?, ?> saveInner = assertInstanceOf(Map.class, world.server.requests(saveEndpoint).get(0).inner());
            assertEquals("1", saveInner.get("executeType"));
            assertTrue(assertInstanceOf(Number.class, saveInner.get("executeTime")).longValue() > 0);

            StandbySchedule badZone =
                    new StandbySchedule(StandbyScheduleRepeat.DAILY)
                            .at("2026-10-24T08:00:00+02:00")
                            .zone("Mars/Olympus_Mons");
            DataException missing =
                    assertInstanceOf(
                            DataException.class,
                            thrown(() -> client.saveStandbyPlan(VIN, plan, new StandbyScheduleOptions().schedule(badZone))));
            assertEquals("ZoneInfoNotFoundError", missing.kind());

            StandbySchedule weekly =
                    new StandbySchedule(StandbyScheduleRepeat.WEEKLY)
                            .startTime("07:30")
                            .weeklyDays(List.of(0L, 1L, 2L, 3L, 4L));
            StandbyPlan newPlan =
                    await(
                            client.createStandbyPlan(
                                    VIN, "Morning", plan.commands(), new StandbyScheduleOptions().schedule(weekly)));
            assertEquals("202", newPlan.id());

            assertEquals(2, await(client.setDefaultStandbyPlan(VIN, "202")).plans().size());
            assertEquals(1, await(client.deleteStandbyPlan(VIN, "202")).plans().size());

            StandbyPlanRunResult run =
                    await(
                            client.runStandbyPlan(
                                    VIN, "101", new RunStandbyPlanOptions().pollAttempts(2).pollInterval(0.01)));
            assertEquals(2L, run.res());
            assertEquals(VIN, run.vin());
        }
    }
}
