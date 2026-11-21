package io.github.pho3nix90.openbyd;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.core.type.TypeReference;
import io.github.pho3nix90.openbyd.model.BydConfig;
import io.github.pho3nix90.openbyd.model.SessionTokens;
import io.github.pho3nix90.openbyd.model.VehicleSnapshot;
import java.lang.ref.Cleaner;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.function.BiConsumer;

/**
 * The async client for the BYD API: pyBYD's {@code BydClient} with a Rust core.
 *
 * <pre>{@code
 * BydConfig config = new BydConfig("user@example.com", "secret").controlPin("123456");
 * try (Client client = new Client(config)) {
 *     client.login().get();
 *     for (Vehicle vehicle : client.getVehicles().get()) {
 *         VehicleRealtimeData realtime = client.getVehicleRealtime(vehicle.vin()).get();
 *         System.out.println(vehicle.vin() + " " + realtime.elecPercent());
 *     }
 * }
 * }</pre>
 *
 * <p>Methods are pyBYD's, in camelCase. A method that waits on the network returns a {@link
 * CompletableFuture}; one that answers at once returns its value. What they return is pyBYD's
 * models as records. Failures are pyBYD's exception classes ({@link BydApiException} and its
 * subclasses, with {@code code()} and {@code endpoint()}): a future is completed with one, and a
 * method that answers at once throws it.
 *
 * <p>Every client in a process shares one HTTP connection pool and one Rust runtime, so a client
 * is cheap: thousands can be made. A client is safe to use from any thread.
 *
 * <p>Callbacks run on one thread the client keeps for them, and what is chained to a future runs
 * on the executor futures use by default: never on a thread of the core. A callback that throws
 * is reported to {@link ClientOptions#onCallbackError} and does not affect the client.
 *
 * <p>Close a client when done with it. One that is dropped without being closed is closed when
 * it is collected, unless it listens for pushes or has a call in flight, which keep it alive.
 */
public final class Client extends ClientMethods implements AutoCloseable {
    private static final Cleaner CLEANER = Cleaner.create();
    private static final TypeReference<BuiltCar> BUILT_CAR = new TypeReference<>() {};

    /** What {@code get_car} answers: the car's number in the core, and its VIN. */
    record BuiltCar(@JsonProperty("car") long car, @JsonProperty("vin") String vin) {}

    private final Core core;
    private final Cleaner.Cleanable cleanable;
    private final Map<Long, Car> cars = new HashMap<>();

    /**
     * A client for an account, ready to use.
     *
     * @throws ValidationException when a config value does not validate
     */
    public Client(BydConfig config) {
        this(config, new ClientOptions());
    }

    /**
     * A client for an account with callbacks and settings, ready to use. A client with push
     * listeners ({@code onVehicleInfo}, {@code onMqttEvent}, {@code onCommandAck}, {@code
     * onCommandLifecycle}, {@code onMqttConnect}, or a car's {@code onStateChanged}) stays alive
     * until it is closed.
     *
     * @throws ValidationException when a config value does not validate
     * @throws DataException when an option has a value that cannot be used
     */
    public Client(BydConfig config, ClientOptions options) {
        this(new Link(new Core(config, options), null, null));
    }

    private Client(Link link) {
        super(link);
        this.core = link.core();
        link.client(this);
        // The action must not refer to this client, or it would never be collected.
        this.cleanable = CLEANER.register(this, core::dispose);
        try {
            core.start(this);
        } catch (RuntimeException | Error error) {
            cleanable.clean();
            throw error;
        }
    }

    /**
     * A client that resumes a session from {@link #exportSession}, without a password login.
     *
     * @see #fromSession(SessionTokens, BydConfig, ClientOptions)
     */
    public static CompletableFuture<Client> fromSession(SessionTokens session, BydConfig config) {
        return fromSession(session, config, new ClientOptions());
    }

    /**
     * A client that resumes a session from {@link #exportSession}, without a password login.
     *
     * <p>With an empty password in the config ({@code new BydConfig(username, "")}) it never
     * logs in by itself ({@code relogin} defaults to {@link ReloginPolicy#NEVER}): when the
     * session expires, calls fail with {@link BydSessionExpiredException} until the application
     * imports a new one.
     *
     * @return a future of the client, failed with what was wrong with the config or the session
     */
    public static CompletableFuture<Client> fromSession(
            SessionTokens session, BydConfig config, ClientOptions options) {
        CompletableFuture<Client> restored = new CompletableFuture<>();
        Client client;
        try {
            ClientOptions used = options.copy();
            if (used.relogin == null) {
                boolean noPassword = Json.MAPPER.valueToTree(config).path("password").asText("").isEmpty();
                used.relogin = noPassword ? ReloginPolicy.NEVER : ReloginPolicy.PASSWORD;
            }
            client = new Client(config, used);
        } catch (RuntimeException error) {
            restored.completeExceptionally(error);
            return restored;
        }
        client.importSession(session).whenComplete((nothing, error) -> {
            if (error == null) {
                restored.complete(client);
            } else {
                client.close();
                restored.completeExceptionally(error);
            }
        });
        return restored;
    }

    /** Whether {@link #close} was called. */
    public boolean closed() {
        return core.closed();
    }

    /**
     * Closes the cars, stops MQTT and frees the client (pyBYD's {@code async_close}). Later calls
     * fail with {@link BydException}, and calls still in flight with {@link CancelledException}.
     * No callback starts after this returns. Closing twice is fine.
     *
     * <p>A closed client stays closed: make a new one to go on.
     */
    @Override
    public void close() {
        synchronized (cars) {
            cars.clear();
        }
        cleanable.clean();
    }

    /**
     * The {@link Car} of a vehicle, built once and kept.
     *
     * @param vin The vehicle identification number.
     * @see #getCar(String, GetCarOptions)
     */
    public CompletableFuture<Car> getCar(String vin) {
        return getCar(vin, new GetCarOptions());
    }

    /**
     * The {@link Car} of a vehicle, built once and kept.
     *
     * <p>As pyBYD: a kept car is returned as it is, whatever the options. Otherwise the vehicle
     * list is fetched unless {@code vehicle} is set, then the capabilities.
     *
     * @param vin The vehicle identification number.
     * @param options What to change from the defaults.
     */
    public CompletableFuture<Car> getCar(String vin, GetCarOptions options) {
        BiConsumer<String, VehicleSnapshot> listener = options.onStateChanged;
        Map<String, Object> args =
                Invoker.args(
                        "vin", vin,
                        "vehicle", options.vehicle,
                        "projection_ttl", options.projectionTtl,
                        "state_events", listener != null);
        return invoker.call("get_car", args, null, BUILT_CAR).thenApply(built -> {
            synchronized (cars) {
                Car car = cars.get(built.car());
                if (car == null) {
                    car = new Car(new Link(core, this, built.car()), built.vin());
                    cars.put(built.car(), car);
                    if (listener != null) {
                        core.onStateChanged(built.vin(), listener);
                        core.hold(this);
                    }
                }
                return car;
            }
        });
    }
}
