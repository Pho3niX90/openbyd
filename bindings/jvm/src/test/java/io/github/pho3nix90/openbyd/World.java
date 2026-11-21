package io.github.pho3nix90.openbyd;

import io.github.pho3nix90.openbyd.model.BydConfig;
import io.github.pho3nix90.openbyd.model.SessionTokens;
import io.github.pho3nix90.openbyd.testing.Account;
import io.github.pho3nix90.openbyd.testing.FakeServer;
import io.github.pho3nix90.openbyd.testing.Reply;
import io.github.pho3nix90.openbyd.testing.TestBroker;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.function.BooleanSupplier;

/** A fake BYD server and a test broker wired together, and clients pointed at both. */
final class World implements AutoCloseable {
    static final String VIN = "TESTVIN0000000001";
    static final String USER = "user@example.com";
    static final String PASSWORD = "secret";
    static final String PIN = "123456";

    static final String BROKER_LOOKUP = "/app/emqAuth/getEmqBrokerIp";
    static final String VEHICLES = "/app/account/getAllListByUserId";
    static final String LATEST_CONFIG = "/vehicle/vehicleswitch/getLatestConfig";
    static final String VERIFY = "/vehicle/vehicleswitch/verifyControlPassword";
    static final String HVAC = "/control/getStatusNow";
    static final String LOGIN = "/app/account/login";
    static final String REMOTE = "/control/remoteControl";
    static final String REMOTE_RESULT = "/control/remoteControlResult";
    static final String REALTIME = "/vehicleInfo/vehicle/vehicleRealTimeRequest";
    static final String HOMEPAGE = "/control/smartCharge/homePage";
    static final String GPS = "/control/getGpsInfo";

    static final String[] FUNCTIONS = {"1001", "1005", "1006", "1012", "1020", "1021", "1026", "1027", "1030", "1033"};

    final FakeServer server = new FakeServer(new Account(USER, PASSWORD));
    final TestBroker broker = new TestBroker();
    private final List<Client> clients = new ArrayList<>();

    World() {
        server.reply(BROKER_LOOKUP, Reply.data(Map.of("emqBorker", broker.address())));
        server.reply(VEHICLES, Reply.data(List.of(Map.of("vin", VIN, "energyType", "0", "modelName", "SEAL"))));
        server.reply(LATEST_CONFIG, Reply.data(Map.of(VIN, functions(FUNCTIONS))));
        server.reply(VERIFY, Reply.data(Map.of("ok", true)));
        server.reply(HVAC, Reply.data(Map.of("acSwitch", 1, "tempInCar", 21)));
    }

    /** A car's capability data that lists these function numbers. */
    static Map<String, Object> functions(String... numbers) {
        return Map.of("cfFixedList", Arrays.stream(numbers).map(number -> Map.of("functionNo", number)).toList());
    }

    /** The config every client of this world starts from: MQTT off, the test account. */
    BydConfig config() {
        return config(PASSWORD);
    }

    BydConfig config(String password) {
        return new BydConfig(USER, password)
                .baseUrl(server.url())
                .mqttEnabled(false)
                .mqttTimeout(5)
                .controlPin(PIN);
    }

    /** Options that trust the test broker. */
    static ClientOptions options() {
        return TestBroker.trust(new ClientOptions());
    }

    /** A client that is closed with the world. */
    Client client() {
        return client(config(), options());
    }

    Client client(ClientOptions options) {
        return client(config(), options);
    }

    Client client(BydConfig config) {
        return client(config, options());
    }

    Client client(BydConfig config, ClientOptions options) {
        return keep(new Client(config, TestBroker.trust(options)));
    }

    Client keep(Client client) {
        synchronized (clients) {
            clients.add(client);
        }
        return client;
    }

    SessionTokens session(Client client) {
        SessionTokens session = client.exportSession();
        if (session == null) {
            throw new IllegalStateException("the client has no session");
        }
        return session;
    }

    void push(Client client, Map<String, Object> body) {
        int reached = broker.push(session(client), body);
        if (reached != 1) {
            throw new IllegalStateException("the push reached " + reached + " connections");
        }
    }

    void untilSubscribed(Client client) {
        String topic = "oversea/res/" + session(client).userId();
        until("a subscription", () -> broker.subscribers(topic) > 0);
    }

    @Override
    public void close() {
        synchronized (clients) {
            clients.forEach(Client::close);
        }
        server.close();
        broker.close();
    }

    static void sleep(long millis) {
        try {
            Thread.sleep(millis);
        } catch (InterruptedException interrupted) {
            Thread.currentThread().interrupt();
            throw new IllegalStateException(interrupted);
        }
    }

    /** Waits until {@code condition} holds. */
    static void until(String what, BooleanSupplier condition) {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (!condition.getAsBoolean()) {
            if (System.nanoTime() > deadline) {
                throw new AssertionError("timed out waiting for " + what);
            }
            sleep(5);
        }
    }

    /** What a future completes with; what it fails with is thrown as it is. */
    static <T> T await(CompletableFuture<T> future) throws Exception {
        try {
            return future.get(30, TimeUnit.SECONDS);
        } catch (ExecutionException failed) {
            if (failed.getCause() instanceof Exception cause) {
                throw cause;
            }
            throw failed;
        }
    }

    /** What {@code run} throws, or the future it returns fails with; fails when nothing does. */
    static Throwable thrown(Callable<?> run) {
        try {
            Object result = run.call();
            if (result instanceof CompletableFuture<?> future) {
                future.get(30, TimeUnit.SECONDS);
            }
        } catch (ExecutionException failed) {
            return failed.getCause();
        } catch (Exception error) {
            return error;
        }
        throw new AssertionError("nothing was thrown");
    }
}
