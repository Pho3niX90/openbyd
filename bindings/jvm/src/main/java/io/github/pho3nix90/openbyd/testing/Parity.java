package io.github.pho3nix90.openbyd.testing;

import io.github.pho3nix90.openbyd.Internal;

/**
 * The parity hook: what the repository's tests compare openbyd with pyBYD through. pyBYD's answer
 * for the same input is recorded in the repository's vectors. Not part of the public API.
 */
public final class Parity {
    private Parity() {}

    /**
     * Runs one parity call of the core.
     *
     * @param name The call, such as {@code hashing.md5_hex}.
     * @param args The call's arguments, as JSON text. They reach the core as written, so a
     *     number keeps its exact value and its kind ({@code 1} or {@code 1.0}).
     * @return the outcome as JSON text: {@code {"ok": ...}} or {@code {"error": {"kind":
     *     ..., "message": ...}}}, where {@code kind} is the Python exception class pyBYD raises
     */
    public static String call(String name, String args) {
        String request = "{\"name\":" + Calls.json(name) + ",\"args\":" + args + "}";
        return Internal.call("parity_call", request);
    }
}
