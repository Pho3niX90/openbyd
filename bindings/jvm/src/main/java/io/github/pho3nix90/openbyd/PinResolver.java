package io.github.pho3nix90.openbyd;

import org.jspecify.annotations.Nullable;

/**
 * Returns the {@code commandPwd} to send with a command, in place of pyBYD's rule (32 hex digits
 * are a digest, anything else is hashed).
 *
 * <p>It is asked on the client's callback thread. If it throws, pyBYD's rule is used and the
 * failure is reported to {@link ClientOptions#onCallbackError}.
 */
@FunctionalInterface
public interface PinResolver {
    /**
     * The {@code commandPwd} to send; empty for none.
     *
     * @param commandPwd The PIN the call was given, if any.
     * @param controlPin The config's {@code controlPin}, if any.
     */
    String resolve(@Nullable String commandPwd, @Nullable String controlPin);
}
