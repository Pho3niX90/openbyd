package io.github.pho3nix90.openbyd.testing;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.JsonNode;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import org.jspecify.annotations.Nullable;

/** A fake BYD server on 127.0.0.1 that speaks the real wire protocol. Close it to stop it. */
public final class FakeServer implements AutoCloseable {
    private static final TypeReference<List<RecordedRequest>> REQUESTS = new TypeReference<>() {};

    private final long server;
    private final String url;

    /** Starts a server that knows these accounts. */
    public FakeServer(Account... accounts) {
        List<Map<String, Object>> known = Arrays.stream(accounts).map(Account::args).toList();
        JsonNode started = Calls.call("server.start", Calls.args("accounts", known));
        this.server = started.path("server").asLong();
        this.url = started.path("url").asText();
    }

    /** Whether the native library loaded has the fake server and the test broker. */
    public static boolean isAvailable() {
        return Calls.tree(io.github.pho3nix90.openbyd.Internal.call("has_testing", "{}")).asBoolean();
    }

    /** The server's base URL, for a config's {@code baseUrl}. */
    public String url() {
        return url;
    }

    /** Makes another account known. */
    public void addAccount(Account account) {
        Map<String, Object> args = account.args();
        args.put("server", server);
        Calls.call("server.add_account", args);
    }

    /** Answers every request to {@code endpoint} with {@code reply}. */
    public void reply(String endpoint, Reply reply) {
        Calls.call("server.reply", Calls.args("server", server, "endpoint", endpoint, "reply", reply.fields()));
    }

    /** Answers the next requests to {@code endpoint} with {@code replies}, one each. */
    public void script(String endpoint, Reply... replies) {
        List<Map<String, @Nullable Object>> scripted = Arrays.stream(replies).map(Reply::fields).toList();
        Calls.call("server.script", Calls.args("server", server, "endpoint", endpoint, "replies", scripted));
    }

    /** The requests received, to every endpoint. */
    public List<RecordedRequest> requests() {
        return Calls.MAPPER.convertValue(Calls.call("server.requests", Calls.args("server", server)), REQUESTS);
    }

    /** The requests received to one endpoint. */
    public List<RecordedRequest> requests(String endpoint) {
        JsonNode requests = Calls.call("server.requests", Calls.args("server", server, "endpoint", endpoint));
        return Calls.MAPPER.convertValue(requests, REQUESTS);
    }

    /** Forgets the requests received. */
    public void clearRequests() {
        Calls.call("server.clear_requests", Calls.args("server", server));
    }

    /** Makes the server refuse every session of an account, as an expired one. */
    public void expireSessions(String username) {
        Calls.call("server.expire_sessions", Calls.args("server", server, "username", username));
    }

    /** The user ID of an account, or null when the server does not know it. */
    public @Nullable String userId(String username) {
        JsonNode id = Calls.call("server.user_id", Calls.args("server", server, "username", username));
        return id.isTextual() ? id.textValue() : null;
    }

    /** Stops the server. Stopping twice is fine. */
    @Override
    public void close() {
        Calls.call("server.stop", Calls.args("server", server));
    }
}
