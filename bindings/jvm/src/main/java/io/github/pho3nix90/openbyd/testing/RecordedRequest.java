package io.github.pho3nix90.openbyd.testing;

import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.List;
import java.util.Map;
import org.jspecify.annotations.Nullable;

/**
 * One request the fake server received, decoded.
 *
 * @param endpoint The endpoint the request was sent to.
 * @param headers The request's headers, each a name and a value.
 * @param cookies The request's cookies, each a name and a value.
 * @param body The request's body as it was sent.
 * @param outer The outer payload, when the envelope opened.
 * @param inner The decrypted inner payload, when it decrypted.
 * @param username The account the request was verified as.
 * @param code The API code answered; null for an HTTP-level reply.
 * @param status The HTTP status answered; null when the connection was dropped.
 */
public record RecordedRequest(
        @JsonProperty("endpoint") String endpoint,
        @JsonProperty("headers") List<List<String>> headers,
        @JsonProperty("cookies") List<List<String>> cookies,
        @JsonProperty("body") String body,
        @JsonProperty("outer") @Nullable Map<String, @Nullable Object> outer,
        @JsonProperty("inner") @Nullable Object inner,
        @JsonProperty("username") @Nullable String username,
        @JsonProperty("code") @Nullable String code,
        @JsonProperty("status") @Nullable Integer status) {}
