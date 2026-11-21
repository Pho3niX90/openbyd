package io.github.pho3nix90.openbyd.testing;

import com.fasterxml.jackson.databind.JsonNode;
import io.github.pho3nix90.openbyd.ClientOptions;
import io.github.pho3nix90.openbyd.Internal;
import io.github.pho3nix90.openbyd.model.SessionTokens;
import java.util.Map;

/**
 * A TLS MQTT broker on 127.0.0.1, which clients made with options that {@link #trust trust} it
 * connect to. Close it to stop it.
 */
public final class TestBroker implements AutoCloseable {
    private final long broker;
    private final String address;
    private final int port;

    /** Starts a broker on a free port. */
    public TestBroker() {
        JsonNode started = Calls.call("broker.start", Map.of());
        this.broker = started.path("broker").asLong();
        this.address = started.path("address").asText();
        this.port = started.path("port").asInt();
    }

    /**
     * Makes clients made with {@code options} trust the test broker's certificate. For tests
     * only: it needs a native library built with the feature {@code testing}.
     *
     * @return the options, to chain
     */
    public static ClientOptions trust(ClientOptions options) {
        Internal.trustTestBroker(options);
        return options;
    }

    /** The address as BYD's broker lookup gives it: reply {@code {"emqBorker": address}}. */
    public String address() {
        return address;
    }

    /** The port the broker listens on. */
    public int port() {
        return port;
    }

    /**
     * Sends {@code body} to the account of {@code session} as BYD does: on its topic, encrypted
     * with the session's content key.
     *
     * @return how many connections it reached
     */
    public int push(SessionTokens session, Object body) {
        Map<String, String> account = Map.of("user_id", session.userId(), "encry_token", session.encryToken());
        return Calls.call("broker.push", Calls.args("broker", broker, "session", account, "body", body)).asInt();
    }

    /** Connections subscribed to {@code topic}. */
    public int subscribers(String topic) {
        return Calls.call("broker.subscribers", Calls.args("broker", broker, "topic", topic)).asInt();
    }

    /** Connections still open on the broker's side. */
    public int open() {
        return Calls.call("broker.open", Calls.args("broker", broker)).asInt();
    }

    /** How many connection attempts the broker has seen. */
    public int connects() {
        return Calls.call("broker.connects", Calls.args("broker", broker)).asInt();
    }

    /** Refuses the next {@code count} connection attempts. */
    public void refuseNext(int count) {
        Calls.call("broker.refuse_next", Calls.args("broker", broker, "count", count));
    }

    /** Drops every connection. */
    public void dropAll() {
        Calls.call("broker.drop_all", Calls.args("broker", broker));
    }

    /** Stops the broker. Stopping twice is fine. */
    @Override
    public void close() {
        Calls.call("broker.stop", Calls.args("broker", broker));
    }
}
