package io.github.pho3nix90.openbyd.testing;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.github.pho3nix90.openbyd.Internal;
import java.util.LinkedHashMap;
import java.util.Map;
import org.jspecify.annotations.Nullable;

/** The fake server's and the test broker's calls, by name. */
final class Calls {
    static final ObjectMapper MAPPER = new ObjectMapper();

    private Calls() {}

    /** Arguments from names and values in turn; a null value is left out. */
    static Map<String, Object> args(@Nullable Object... pairs) {
        Map<String, Object> args = new LinkedHashMap<>();
        for (int index = 0; index < pairs.length; index += 2) {
            Object value = pairs[index + 1];
            if (value != null) {
                args.put((String) pairs[index], value);
            }
        }
        return args;
    }

    static String json(Object value) {
        try {
            return MAPPER.writeValueAsString(value);
        } catch (JsonProcessingException error) {
            throw new IllegalArgumentException("a value cannot be written as JSON: " + error.getMessage(), error);
        }
    }

    static JsonNode tree(String json) {
        try {
            return MAPPER.readTree(json);
        } catch (JsonProcessingException error) {
            throw new IllegalStateException("the native library answered with text that is not JSON", error);
        }
    }

    /** Runs the testing call {@code name} and returns its answer. */
    static JsonNode call(String name, Map<String, Object> args) {
        return tree(Internal.testingCall(name, json(args)));
    }
}
