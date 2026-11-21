package io.github.pho3nix90.openbyd;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.JsonNode;
import com.sun.jna.CallbackThreadInitializer;
import com.sun.jna.Native;
import com.sun.jna.Pointer;
import com.sun.jna.ptr.PointerByReference;
import io.github.pho3nix90.openbyd.model.BydConfig;
import io.github.pho3nix90.openbyd.model.CommandAckEvent;
import io.github.pho3nix90.openbyd.model.CommandGateVerdict;
import io.github.pho3nix90.openbyd.model.CommandLifecycleEvent;
import io.github.pho3nix90.openbyd.model.RequestRecord;
import io.github.pho3nix90.openbyd.model.SessionTokens;
import io.github.pho3nix90.openbyd.model.VehicleRealtimeData;
import io.github.pho3nix90.openbyd.model.VehicleSnapshot;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.function.BiConsumer;
import org.jspecify.annotations.Nullable;

/**
 * A client's connection to the core: calls out, events in.
 *
 * <p>The core calls back on its own threads, which must get away at once. An event is handed to
 * the one thread this client keeps for its callbacks, so they run in the order things happened;
 * the outcome of a call is handed to the executor futures use by default, so whatever is chained
 * to a future never runs on a thread of the core either.
 *
 * <p>The native client is freed once, by {@link #dispose}. Calls hold the read side of a lock
 * and freeing the write side, so nothing uses the native client while or after it is freed.
 */
final class Core {
    private static final System.Logger LOG = System.getLogger("io.github.pho3nix90.openbyd");
    private static final TypeReference<Void> NOTHING = new TypeReference<>() {};
    private static final TypeReference<Boolean> BOOL = new TypeReference<>() {};
    private static final TypeReference<Map<String, @Nullable Object>> OBJECT = new TypeReference<>() {};

    /** Every client whose native client lives, by the number the core hands back to the callbacks. */
    private static final ConcurrentHashMap<Long, Core> LIVE = new ConcurrentHashMap<>();

    private static final AtomicLong NEXT_ID = new AtomicLong();

    // The callbacks are shared by every client and never collected: the core
    // may call them for as long as any native client lives.
    private static final Lib.EventFn ON_EVENT = new OnEvent();
    private static final Lib.DoneFn ON_DONE = new OnDone();

    static {
        // The core's threads live as long as the process: attach each once, as a daemon.
        CallbackThreadInitializer workers = new CallbackThreadInitializer(true, false, "openbyd-worker");
        Native.setCallbackThreadInitializer(ON_EVENT, workers);
        Native.setCallbackThreadInitializer(ON_DONE, workers);
    }

    private static final class OnEvent implements Lib.EventFn {
        @Override
        public void invoke(@Nullable Pointer userData, Pointer name, Pointer payload) {
            try {
                Core core = LIVE.get(Pointer.nativeValue(userData));
                if (core != null) {
                    core.enqueue(Lib.text(name), Lib.text(payload));
                }
            } catch (Throwable error) {
                // Nothing may be thrown into the core.
                LOG.log(System.Logger.Level.ERROR, "an event of the core was lost", error);
            }
        }
    }

    private static final class OnDone implements Lib.DoneFn {
        @Override
        public void invoke(@Nullable Pointer userData, long call, int ok, Pointer json) {
            try {
                Core core = LIVE.get(Pointer.nativeValue(userData));
                Pending pending = core == null ? null : core.pending.remove(call);
                if (pending != null) {
                    pending.finishLater(ok != 0, Lib.text(json));
                }
            } catch (Throwable error) {
                LOG.log(System.Logger.Level.ERROR, "the outcome of a call was lost", error);
            }
        }
    }

    /** A callback of the application: it may throw anything. */
    @FunctionalInterface
    private interface Action {
        void run() throws Exception;
    }

    /** A question's callback: what it returns is the answer. */
    @FunctionalInterface
    private interface Question {
        @Nullable Object answer() throws Exception;
    }

    /**
     * A call in flight. It holds {@code keepAlive}, the client or car the call was made on, so
     * that one is not collected, and its native client freed, before the call ends.
     */
    private record Pending(
            CompletableFuture<Object> future, TypeReference<?> type, String method, @Nullable Object keepAlive) {
        void finish(boolean ok, String json) {
            if (!ok) {
                future.completeExceptionally(Json.error(json));
                return;
            }
            try {
                future.complete(Json.decode(json, type, method));
            } catch (RuntimeException error) {
                future.completeExceptionally(error);
            }
        }

        void finishLater(boolean ok, String json) {
            try {
                future.defaultExecutor().execute(() -> finish(ok, json));
            } catch (RejectedExecutionException shuttingDown) {
                finish(ok, json);
            }
        }
    }

    private final long id = NEXT_ID.incrementAndGet();
    private final ClientOptions options;
    private final boolean listens;
    private final ReentrantReadWriteLock lock = new ReentrantReadWriteLock();
    private final ConcurrentHashMap<Long, Pending> pending = new ConcurrentHashMap<>();
    private final AtomicLong nextCall = new AtomicLong();
    private final ThreadPoolExecutor events;

    /** The state listener of each car, by VIN. */
    private final ConcurrentHashMap<String, BiConsumer<String, VehicleSnapshot>> stateListeners =
            new ConcurrentHashMap<>();

    /** The native client; null once freed. Guarded by {@link #lock}. */
    private @Nullable Pointer client;

    private volatile boolean closed;

    /** The client while it listens for pushes: it is not collected while it does. */
    @SuppressWarnings("unused")
    private volatile @Nullable Object listener;

    /** A native client for {@code config} with {@code options}, closed until {@link #start}. */
    Core(BydConfig config, ClientOptions options) {
        this.options = options.copy();
        List<String> wanted = new ArrayList<>();
        addIf(wanted, "vehicle_info", options.onVehicleInfo);
        addIf(wanted, "mqtt_event", options.onMqttEvent);
        addIf(wanted, "command_ack", options.onCommandAck);
        addIf(wanted, "command_lifecycle", options.onCommandLifecycle);
        addIf(wanted, "mqtt_connect", options.onMqttConnect);
        // A client with one of these waits for pushes between calls. A request is only seen during one.
        this.listens = !wanted.isEmpty();
        addIf(wanted, "request", options.requestObserver);

        Map<String, @Nullable Object> host = new LinkedHashMap<>();
        host.put("events", wanted);
        host.put("command_ack_ttl_seconds", options.commandAckTtlSeconds);
        host.put("command_retries", options.commandRetries);
        host.put("gate_policy", gatePolicy(options.gatePolicy));
        host.put("pin_resolver", options.pinResolver == null ? null : "host");
        host.put("relogin", relogin(options.relogin));
        host.put("test_tls", options.testTls ? Boolean.TRUE : null);

        // One daemon thread at a time, started by the first event and gone when idle.
        // Its factory must not refer to this client, which an idle thread would then keep.
        String threadName = "openbyd-callbacks-" + id;
        this.events =
                new ThreadPoolExecutor(
                        0,
                        1,
                        10,
                        TimeUnit.SECONDS,
                        new LinkedBlockingQueue<>(),
                        task -> {
                            Thread thread = new Thread(task, threadName);
                            thread.setDaemon(true);
                            return thread;
                        });

        String configJson = Json.encode(Map.of(), config);
        String hostJson = Json.encode(host, null);
        PointerByReference error = new PointerByReference();
        Pointer made = Lib.c().openbyd_client_new(configJson, hostJson, ON_EVENT, ON_DONE, new Pointer(id), error);
        if (made == null) {
            Pointer reported = error.getValue();
            throw reported == null
                    ? new BydException("the client could not be made")
                    : Json.error(Lib.take(reported));
        }
        this.client = made;
        LIVE.put(id, this);
    }

    private static void addIf(List<String> events, String event, @Nullable Object callback) {
        if (callback != null) {
            events.add(event);
        }
    }

    private static @Nullable String gatePolicy(@Nullable GatePolicy policy) {
        if (policy == null) {
            return null;
        }
        if (policy == GatePolicy.STRICT) {
            return "strict";
        }
        return policy == GatePolicy.ALLOW_MISSING_DATA ? "allow_missing_data" : "host";
    }

    private static @Nullable String relogin(@Nullable ReloginPolicy policy) {
        if (policy == null) {
            return null;
        }
        if (policy == ReloginPolicy.PASSWORD) {
            return "password";
        }
        return policy == ReloginPolicy.NEVER ? "never" : "host";
    }

    /** How many native clients live: made and not yet freed. */
    static int live() {
        return LIVE.size();
    }

    private static BydException closedError() {
        return new BydException("the client is closed");
    }

    // -- calls ---------------------------------------------------------------

    /**
     * Starts a method that waits on the network. Whatever fails, also before anything is sent,
     * completes the future.
     */
    <T extends @Nullable Object> CompletableFuture<T> call(
            @Nullable Object keepAlive,
            String method,
            Map<String, ? extends @Nullable Object> args,
            @Nullable Object extra,
            TypeReference<T> type) {
        CompletableFuture<T> future = new CompletableFuture<>();
        String text;
        try {
            text = Json.encode(args, extra);
        } catch (RuntimeException error) {
            future.completeExceptionally(error);
            return future;
        }
        long call = nextCall.incrementAndGet();
        @SuppressWarnings("unchecked")
        CompletableFuture<Object> erased = (CompletableFuture<Object>) future;
        pending.put(call, new Pending(erased, type, method, keepAlive));
        lock.readLock().lock();
        try {
            Pointer open = client;
            if (open == null) {
                pending.remove(call);
                future.completeExceptionally(closedError());
            } else {
                Lib.c().openbyd_client_call(open, call, method, text);
            }
        } finally {
            lock.readLock().unlock();
        }
        return future;
    }

    /** Runs a method that answers at once. */
    <T extends @Nullable Object> T callSync(
            String method,
            Map<String, ? extends @Nullable Object> args,
            @Nullable Object extra,
            TypeReference<T> type) {
        String text = Json.encode(args, extra);
        Lib.Outcome outcome;
        lock.readLock().lock();
        try {
            Pointer open = client;
            if (open == null) {
                throw closedError();
            }
            outcome = Lib.callSync(open, method, text);
        } finally {
            lock.readLock().unlock();
        }
        return Json.decode(Json.result(outcome), type, method);
    }

    /** Opens the client (pyBYD's {@code async_start}). {@code owner} is the client that listens. */
    void start(Object owner) {
        callSync("start", Map.of(), null, NOTHING);
        if (listens) {
            hold(owner);
        }
    }

    /** Keeps {@code owner} from being collected until the client is closed: it listens for pushes. */
    void hold(Object owner) {
        listener = owner;
    }

    /** Hears every change of the state of the car with this VIN. */
    void onStateChanged(String vin, BiConsumer<String, VehicleSnapshot> heard) {
        stateListeners.put(vin, heard);
    }

    boolean closed() {
        return closed;
    }

    /**
     * Closes the native client and frees it; safe to call again. No callback of the core runs
     * after it returns, and calls still in flight fail with {@link CancelledException}.
     */
    void dispose() {
        Pointer freed;
        lock.writeLock().lock();
        try {
            freed = client;
            client = null;
            closed = true;
            if (freed != null) {
                // Waits for the core's callbacks in progress, which only queue and return.
                Lib.c().openbyd_client_free(freed);
            }
        } finally {
            lock.writeLock().unlock();
        }
        if (freed == null) {
            return;
        }
        LIVE.remove(id);
        listener = null;
        stateListeners.clear();
        events.shutdown();
        for (Long call : pending.keySet()) {
            Pending left = pending.remove(call);
            if (left != null) {
                left.future().completeExceptionally(new CancelledException("the client was closed"));
            }
        }
    }

    // -- events --------------------------------------------------------------

    private void enqueue(String name, String payload) {
        try {
            events.execute(() -> dispatch(name, payload));
        } catch (RejectedExecutionException closing) {
            // The client is being closed: nobody listens any more.
        }
    }

    private static @Nullable String text(JsonNode data, String field) {
        JsonNode value = data.path(field);
        return value.isTextual() ? value.textValue() : null;
    }

    private static String required(JsonNode data, String field) {
        return data.path(field).asText("");
    }

    private void dispatch(String name, String payload) {
        if (closed) {
            return;
        }
        JsonNode data;
        try {
            data = Json.tree(payload, "the event " + name);
        } catch (RuntimeException error) {
            report(name, error);
            return;
        }
        switch (name) {
            case "vehicle_info" -> {
                var heard = options.onVehicleInfo;
                if (heard != null) {
                    tell("onVehicleInfo", () -> heard.accept(
                            required(data, "vin"), Json.convert(data.get("data"), VehicleRealtimeData.class, name)));
                }
            }
            case "mqtt_event" -> {
                var heard = options.onMqttEvent;
                if (heard != null) {
                    tell("onMqttEvent", () -> heard.onMqttEvent(
                            required(data, "event"),
                            required(data, "vin"),
                            Json.MAPPER.convertValue(data.get("data"), OBJECT)));
                }
            }
            case "command_ack" -> {
                var heard = options.onCommandAck;
                if (heard != null) {
                    tell("onCommandAck", () -> heard.accept(Json.convert(data.get("ack"), CommandAckEvent.class, name)));
                }
            }
            case "command_lifecycle" -> {
                var heard = options.onCommandLifecycle;
                if (heard != null) {
                    tell("onCommandLifecycle", () -> heard.accept(
                            Json.convert(data.get("event"), CommandLifecycleEvent.class, name)));
                }
            }
            case "mqtt_connect" -> {
                var heard = options.onMqttConnect;
                if (heard != null) {
                    tell("onMqttConnect", heard::run);
                }
            }
            case "request" -> {
                var heard = options.requestObserver;
                if (heard != null) {
                    tell("requestObserver", () -> heard.accept(Json.convert(data.get("record"), RequestRecord.class, name)));
                }
            }
            case "state_changed" -> {
                String vin = required(data, "vin");
                var heard = stateListeners.get(vin);
                if (heard != null) {
                    tell("onStateChanged", () -> heard.accept(
                            vin, Json.convert(data.get("state"), VehicleSnapshot.class, name)));
                }
            }
            case "gate_policy" -> {
                GatePolicy policy = options.gatePolicy;
                if (policy != null) {
                    ask(data.path("request").asLong(), "gatePolicy", () -> policy.allows(
                            required(data, "vin"), Json.convert(data.get("verdict"), CommandGateVerdict.class, name)));
                }
            }
            case "pin_resolver" -> {
                PinResolver resolver = options.pinResolver;
                if (resolver != null) {
                    ask(data.path("request").asLong(), "pinResolver", () -> resolver.resolve(
                            text(data, "command_pwd"), text(data, "control_pin")));
                }
            }
            case "relogin" -> {
                ReloginPolicy policy = options.relogin;
                if (policy != null) {
                    relogin(data.path("request").asLong(), policy);
                }
            }
            default -> {
                // An event of a newer core.
            }
        }
    }

    /** Runs a callback that only hears; what it throws is reported. */
    private void tell(String name, Action callback) {
        try {
            callback.run();
        } catch (Throwable error) {
            report(name, error);
        }
    }

    /**
     * Runs a callback the core waits for and hands its answer back. What it throws is reported,
     * and answered with an error so the core takes its default.
     */
    private void ask(long request, String name, Question callback) {
        Object answer;
        try {
            answer = callback.answer();
        } catch (Throwable error) {
            report(name, error);
            resolve(request, "error", Json.errorToJson(error));
            return;
        }
        resolve(request, "value", answer);
    }

    /**
     * Asks for a new session. What the policy throws, or its stage fails with, is the failure of
     * the call that needed the session, not a stray error: it is handed back, not reported.
     */
    private void relogin(long request, ReloginPolicy policy) {
        CompletionStage<SessionTokens> session;
        try {
            session = policy.relogin();
        } catch (Throwable error) {
            resolve(request, "error", Json.errorToJson(error));
            return;
        }
        if (session == null) {
            resolve(request, "value", null);
            return;
        }
        session.whenComplete((tokens, error) -> {
            if (error == null) {
                resolve(request, "value", tokens);
            } else {
                resolve(request, "error", Json.errorToJson(cause(error)));
            }
        });
    }

    /** What a stage failed with, without the wrapper a future puts around it. */
    private static Throwable cause(Throwable error) {
        Throwable cause = error.getCause();
        boolean wrapper = error instanceof CompletionException || error instanceof ExecutionException;
        return wrapper && cause != null ? cause : error;
    }

    private void resolve(long request, String key, @Nullable Object answer) {
        try {
            callSync("resolve", Invoker.args("request", request, key, answer), null, BOOL);
        } catch (RuntimeException failed) {
            // The client was closed, or the answer cannot be written. The question
            // must not wait for ever on an answer that will not come.
            if (key.equals("value") && !closed) {
                resolve(request, "error", Json.errorToJson(failed));
            }
        }
    }

    private void report(String name, Throwable error) {
        BiConsumer<String, Throwable> handler = options.onCallbackError;
        if (handler == null) {
            LOG.log(System.Logger.Level.WARNING, "the " + name + " callback threw: " + error, error);
            return;
        }
        try {
            handler.accept(name, error);
        } catch (Throwable ignored) {
            // A failing error handler has nowhere to report to.
        }
    }
}
