package io.github.pho3nix90.openbyd;

import io.github.pho3nix90.openbyd.model.CommandAckEvent;
import io.github.pho3nix90.openbyd.model.CommandLifecycleEvent;
import io.github.pho3nix90.openbyd.model.RequestRecord;
import io.github.pho3nix90.openbyd.model.VehicleRealtimeData;
import java.util.Map;
import java.util.Objects;
import java.util.function.BiConsumer;
import java.util.function.Consumer;
import org.jspecify.annotations.Nullable;

/**
 * The options of a {@link Client} besides pyBYD's config: its callbacks and openbyd's settings.
 *
 * <pre>{@code
 * ClientOptions options = new ClientOptions()
 *         .onVehicleInfo((vin, data) -> System.out.println(vin + " " + data.elecPercent()))
 *         .gatePolicy(GatePolicy.ALLOW_MISSING_DATA)
 *         .commandRetries(2);
 * }</pre>
 *
 * <p>An option that is not set takes its default. Every callback runs on one thread the client
 * keeps for them, in the order things happened, never on a thread of the core. One that throws
 * is reported to {@link #onCallbackError} and does not affect the client.
 */
public final class ClientOptions {
    /** Hears every MQTT event that names a VIN. */
    @FunctionalInterface
    public interface MqttEventListener {
        /**
         * An MQTT event arrived.
         *
         * @param event The event's name, such as {@code vehicleInfo}.
         * @param vin The vehicle identification number.
         * @param data The event's {@code respondData}.
         */
        void onMqttEvent(String event, String vin, Map<String, @Nullable Object> data);
    }

    @Nullable BiConsumer<String, VehicleRealtimeData> onVehicleInfo;
    @Nullable MqttEventListener onMqttEvent;
    @Nullable Consumer<CommandAckEvent> onCommandAck;
    @Nullable Consumer<CommandLifecycleEvent> onCommandLifecycle;
    @Nullable Runnable onMqttConnect;
    @Nullable Consumer<RequestRecord> requestObserver;
    @Nullable Double commandAckTtlSeconds;
    @Nullable Long commandRetries;
    @Nullable GatePolicy gatePolicy;
    @Nullable PinResolver pinResolver;
    @Nullable ReloginPolicy relogin;
    @Nullable BiConsumer<String, Throwable> onCallbackError;
    boolean testTls;

    /** Options with every value left to its default and no callback. */
    public ClientOptions() {}

    ClientOptions copy() {
        ClientOptions copy = new ClientOptions();
        copy.onVehicleInfo = onVehicleInfo;
        copy.onMqttEvent = onMqttEvent;
        copy.onCommandAck = onCommandAck;
        copy.onCommandLifecycle = onCommandLifecycle;
        copy.onMqttConnect = onMqttConnect;
        copy.requestObserver = requestObserver;
        copy.commandAckTtlSeconds = commandAckTtlSeconds;
        copy.commandRetries = commandRetries;
        copy.gatePolicy = gatePolicy;
        copy.pinResolver = pinResolver;
        copy.relogin = relogin;
        copy.onCallbackError = onCallbackError;
        copy.testTls = testTls;
        return copy;
    }

    /**
     * Hears every MQTT {@code vehicleInfo} push, parsed: the VIN and the realtime data.
     *
     * @return this, to chain
     */
    public ClientOptions onVehicleInfo(BiConsumer<String, VehicleRealtimeData> listener) {
        this.onVehicleInfo = Objects.requireNonNull(listener, "listener");
        return this;
    }

    /**
     * Hears every MQTT event that names a VIN, with its raw {@code respondData}.
     *
     * @return this, to chain
     */
    public ClientOptions onMqttEvent(MqttEventListener listener) {
        this.onMqttEvent = Objects.requireNonNull(listener, "listener");
        return this;
    }

    /**
     * Hears every remote-control acknowledgement.
     *
     * @return this, to chain
     */
    public ClientOptions onCommandAck(Consumer<CommandAckEvent> listener) {
        this.onCommandAck = Objects.requireNonNull(listener, "listener");
        return this;
    }

    /**
     * Hears when a command was registered, matched, expired or acknowledged without a match.
     *
     * @return this, to chain
     */
    public ClientOptions onCommandLifecycle(Consumer<CommandLifecycleEvent> listener) {
        this.onCommandLifecycle = Objects.requireNonNull(listener, "listener");
        return this;
    }

    /**
     * Hears every accepted MQTT connection.
     *
     * @return this, to chain
     */
    public ClientOptions onMqttConnect(Runnable listener) {
        this.onMqttConnect = Objects.requireNonNull(listener, "listener");
        return this;
    }

    /**
     * Hears every request the client sends, with its outcome and duration.
     *
     * @return this, to chain
     */
    public ClientOptions requestObserver(Consumer<RequestRecord> observer) {
        this.requestObserver = Objects.requireNonNull(observer, "observer");
        return this;
    }

    /**
     * How long a command waits for its acknowledgement, in seconds. Default {@code 300}.
     *
     * @return this, to chain
     */
    public ClientOptions commandAckTtlSeconds(double seconds) {
        this.commandAckTtlSeconds = seconds;
        return this;
    }

    /**
     * Attempts per remote command. Default {@code 3}.
     *
     * @return this, to chain
     */
    public ClientOptions commandRetries(int attempts) {
        this.commandRetries = (long) attempts;
        return this;
    }

    /**
     * Whether a command is sent, given the command gate's verdict. Default {@link
     * GatePolicy#STRICT}.
     *
     * @return this, to chain
     */
    public ClientOptions gatePolicy(GatePolicy policy) {
        this.gatePolicy = Objects.requireNonNull(policy, "policy");
        return this;
    }

    /**
     * Replaces pyBYD's rule for the {@code commandPwd} sent with a command.
     *
     * @return this, to chain
     */
    public ClientOptions pinResolver(PinResolver resolver) {
        this.pinResolver = Objects.requireNonNull(resolver, "resolver");
        return this;
    }

    /**
     * What to do when a new session is needed. Default {@link ReloginPolicy#PASSWORD}.
     *
     * @return this, to chain
     */
    public ClientOptions relogin(ReloginPolicy policy) {
        this.relogin = Objects.requireNonNull(policy, "policy");
        return this;
    }

    /**
     * Hears about a callback that threw: its option's name, such as {@code onVehicleInfo}, and
     * what it threw. By default the failure is logged as a warning to the {@link System.Logger}
     * named {@code io.github.pho3nix90.openbyd}.
     *
     * @return this, to chain
     */
    public ClientOptions onCallbackError(BiConsumer<String, Throwable> handler) {
        this.onCallbackError = Objects.requireNonNull(handler, "handler");
        return this;
    }
}
