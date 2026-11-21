package io.github.pho3nix90.openbyd;

/**
 * What the {@code testing} package needs from this one. Not part of the API: it may change in
 * any release.
 *
 * @hidden
 */
public final class Internal {
    private Internal() {}

    /**
     * Runs a function of the core that needs no client, with its arguments as JSON text, and
     * returns its JSON. A failure is thrown as this package's exception.
     */
    public static String call(String function, String args) {
        return Json.result(Lib.call(function, args));
    }

    /**
     * Runs a call of the fake server or the test broker, with its arguments as JSON text, and
     * returns its JSON. Fails in a native library built without the feature {@code testing}.
     */
    public static String testingCall(String name, String args) {
        return Json.result(Lib.testingCall(name, args));
    }

    /** Makes clients made with these options trust the certificate of the MQTT test broker. */
    public static void trustTestBroker(ClientOptions options) {
        options.testTls = true;
    }
}
