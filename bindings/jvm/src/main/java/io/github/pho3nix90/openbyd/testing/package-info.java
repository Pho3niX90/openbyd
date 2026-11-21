/**
 * A fake BYD server and an MQTT test broker, for tests.
 *
 * <p>They need a native library built with the cargo feature {@code testing}; released ones do
 * not have it, and {@link io.github.pho3nix90.openbyd.testing.FakeServer#isAvailable} tells which
 * one is loaded.
 *
 * <pre>{@code
 * try (FakeServer server = new FakeServer(new Account("user@example.com", "secret"))) {
 *     server.reply("/control/getStatusNow", Reply.data(Map.of("acSwitch", 1)));
 *     BydConfig config = new BydConfig("user@example.com", "secret").baseUrl(server.url());
 *     try (Client client = new Client(config)) {
 *         client.getHvacStatus("TESTVIN0000000001").get();
 *     }
 * }
 * }</pre>
 */
@NullMarked
package io.github.pho3nix90.openbyd.testing;

import org.jspecify.annotations.NullMarked;
