package io.github.pho3nix90.openbyd;

import com.fasterxml.jackson.core.type.TypeReference;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import org.jspecify.annotations.Nullable;

/**
 * Runs the core's methods by name: what the generated methods call.
 *
 * <p>{@code args} are the method's arguments under pyBYD's names, {@code options} an options
 * object whose set fields are added to them (or null), and {@code type} what the answer is read
 * as.
 */
interface Invoker {
    /** Starts a method that waits on the network. Every failure completes the future. */
    <T extends @Nullable Object> CompletableFuture<T> call(
            String method,
            Map<String, ? extends @Nullable Object> args,
            @Nullable Object options,
            TypeReference<T> type);

    /** Runs a method that answers at once. */
    <T extends @Nullable Object> T callSync(
            String method,
            Map<String, ? extends @Nullable Object> args,
            @Nullable Object options,
            TypeReference<T> type);

    /** Arguments from names and values in turn: {@code args("vin", vin, "name", name)}. */
    static Map<String, @Nullable Object> args(@Nullable Object... pairs) {
        Map<String, @Nullable Object> args = new LinkedHashMap<>();
        for (int index = 0; index < pairs.length; index += 2) {
            args.put((String) pairs[index], pairs[index + 1]);
        }
        return args;
    }
}
