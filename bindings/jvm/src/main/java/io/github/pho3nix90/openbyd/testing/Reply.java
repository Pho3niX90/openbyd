package io.github.pho3nix90.openbyd.testing;

import java.util.LinkedHashMap;
import java.util.Map;
import org.jspecify.annotations.Nullable;

/** What the fake server answers to one request. */
public final class Reply {
    private final Map<String, @Nullable Object> fields = new LinkedHashMap<>();

    private Reply(String key, @Nullable Object value) {
        fields.put(key, value);
    }

    /** Success: code {@code 0} with {@code data} (maps, lists, strings, numbers) as the encrypted {@code respondData}. */
    public static Reply data(Object data) {
        return new Reply("data", data);
    }

    /** An API error with this code. */
    public static Reply code(String code) {
        return new Reply("code", code);
    }

    /** An API error with this code and message. */
    public static Reply code(String code, String message) {
        Reply reply = new Reply("code", code);
        reply.fields.put("message", message);
        return reply;
    }

    /** This outer reply as it is, inside the envelope only. */
    public static Reply outer(Object outer) {
        return new Reply("outer", outer);
    }

    /** An HTTP status other than 200, with this body. */
    public static Reply status(int status, String body) {
        Reply reply = new Reply("status", status);
        reply.fields.put("body", body);
        return reply;
    }

    /** Closes the connection without answering. */
    public static Reply drop() {
        return new Reply("drop", true);
    }

    /** What the server answers without a handler. */
    public static Reply standard() {
        return new Reply("default", true);
    }

    /**
     * Answer after this many seconds.
     *
     * @return this, to chain
     */
    public Reply delay(double seconds) {
        fields.put("delay", seconds);
        return this;
    }

    /**
     * Add this {@code Set-Cookie} header.
     *
     * @return this, to chain
     */
    public Reply cookie(String header) {
        fields.put("cookie", header);
        return this;
    }

    Map<String, @Nullable Object> fields() {
        return fields;
    }
}
