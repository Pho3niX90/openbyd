package io.github.pho3nix90.openbyd;

import io.github.pho3nix90.openbyd.model.SessionTokens;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.function.Supplier;

/**
 * What to do when a new session is needed: {@link #PASSWORD}, {@link #NEVER}, or a function that
 * produces a session.
 *
 * <p>A function is asked on the client's callback thread and may take as long as it needs: the
 * call that needed the session waits for the stage it returns. What it throws, or what its stage
 * fails with, is what that call fails with.
 */
@FunctionalInterface
public interface ReloginPolicy {
    /** Log in again with the password. The default, and pyBYD's rule. */
    ReloginPolicy PASSWORD = () -> CompletableFuture.failedFuture(new IllegalStateException("the core logs in"));

    /** Never log in: fail with {@link BydSessionExpiredException} (BYD's own expiry code, or {@code relogin_disabled} when there was no session). */
    ReloginPolicy NEVER = () -> CompletableFuture.failedFuture(new IllegalStateException("the core refuses"));

    /** A new session, as {@link Client#exportSession} gives one. */
    CompletionStage<SessionTokens> relogin();

    /**
     * A policy from a function that returns the session itself. It runs on the client's callback
     * thread, which delivers nothing else until it returns.
     */
    static ReloginPolicy blocking(Supplier<SessionTokens> session) {
        return () -> CompletableFuture.completedFuture(session.get());
    }
}
