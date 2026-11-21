package io.github.pho3nix90.openbyd;

import com.fasterxml.jackson.core.type.TypeReference;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import org.jspecify.annotations.Nullable;

/**
 * The invoker of a client, or of one of its cars: every call of a car names the car's number.
 *
 * <p>It refers to the client, so a client is not collected while one of its cars is in use, and
 * hands the client to each call, so it is not collected while a call is in flight either.
 */
final class Link implements Invoker {
    private final Core core;
    private final @Nullable Long car;
    private @Nullable Client client;

    Link(Core core, @Nullable Client client, @Nullable Long car) {
        this.core = core;
        this.client = client;
        this.car = car;
    }

    Core core() {
        return core;
    }

    /** Names the client, once it exists: its own link is made before it. */
    void client(Client made) {
        this.client = made;
    }

    private Map<String, ? extends @Nullable Object> named(Map<String, ? extends @Nullable Object> args) {
        if (car == null) {
            return args;
        }
        Map<String, @Nullable Object> withCar = new LinkedHashMap<>(args);
        withCar.put("car", car);
        return withCar;
    }

    @Override
    public <T extends @Nullable Object> CompletableFuture<T> call(
            String method,
            Map<String, ? extends @Nullable Object> args,
            @Nullable Object options,
            TypeReference<T> type) {
        return core.call(client, method, named(args), options, type);
    }

    @Override
    public <T extends @Nullable Object> T callSync(
            String method,
            Map<String, ? extends @Nullable Object> args,
            @Nullable Object options,
            TypeReference<T> type) {
        return core.callSync(method, named(args), options, type);
    }
}
