package io.github.pho3nix90.openbyd;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.core.json.JsonWriteFeature;
import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.MapperFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.cfg.CoercionAction;
import com.fasterxml.jackson.databind.cfg.CoercionInputShape;
import com.fasterxml.jackson.databind.json.JsonMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.fasterxml.jackson.databind.type.LogicalType;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.function.BiFunction;
import org.jspecify.annotations.Nullable;

/** What crosses between Java and the core: JSON text, and errors. */
final class Json {
    /**
     * Reads the core's JSON as the generated types, and nothing else: an unknown property, a
     * missing one, a null where a primitive is, or a value of another type is an error, not a
     * guess. The strings {@code "NaN"}, {@code "Infinity"} and {@code "-Infinity"} are read as
     * numbers where the field is a floating-point number, which is how the core writes them.
     */
    static final ObjectMapper MAPPER =
            JsonMapper.builder()
                    // What goes to the core is ASCII, whatever the platform's encoding; a string that
                    // is not valid Unicode (half a surrogate pair) is then refused by the core, not altered.
                    .enable(JsonWriteFeature.ESCAPE_NON_ASCII)
                    .enable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES)
                    .enable(DeserializationFeature.FAIL_ON_MISSING_CREATOR_PROPERTIES)
                    .enable(DeserializationFeature.FAIL_ON_NULL_FOR_PRIMITIVES)
                    .disable(DeserializationFeature.ACCEPT_FLOAT_AS_INT)
                    .disable(MapperFeature.ALLOW_COERCION_OF_SCALARS)
                    .withCoercionConfig(
                            LogicalType.Textual,
                            config ->
                                    config.setCoercion(CoercionInputShape.Integer, CoercionAction.Fail)
                                            .setCoercion(CoercionInputShape.Float, CoercionAction.Fail)
                                            .setCoercion(CoercionInputShape.Boolean, CoercionAction.Fail))
                    .build();

    private Json() {}

    /**
     * Arguments as JSON text: {@code args}, then the fields of {@code options} that are set. A
     * number JSON cannot hold (NaN, an infinity) is refused, as pyBYD refuses it, instead of
     * being sent as something else.
     */
    static String encode(Map<String, ? extends @Nullable Object> args, @Nullable Object options) {
        ObjectNode node = MAPPER.createObjectNode();
        try {
            for (Map.Entry<String, ? extends @Nullable Object> entry : args.entrySet()) {
                node.set(entry.getKey(), MAPPER.valueToTree(entry.getValue()));
            }
            if (options != null) {
                JsonNode extra = MAPPER.valueToTree(options);
                if (extra instanceof ObjectNode fields) {
                    node.setAll(fields);
                }
            }
        } catch (IllegalArgumentException error) {
            throw unwritable(error);
        }
        refuseNonFinite(node, "the arguments");
        try {
            return MAPPER.writeValueAsString(node);
        } catch (JsonProcessingException error) {
            throw unwritable(error);
        }
    }

    private static DataException unwritable(Exception error) {
        String message = "an argument cannot be written as JSON: " + error.getMessage();
        return caused(new DataException(message, "TypeError"), error);
    }

    private static void refuseNonFinite(JsonNode node, String key) {
        if (node.isFloatingPointNumber() && !node.isBigDecimal() && !Double.isFinite(node.doubleValue())) {
            throw new DataException(key + " must be a finite number, not " + node.doubleValue(), "ValueError");
        }
        if (node.isObject()) {
            node.properties().forEach(entry -> refuseNonFinite(entry.getValue(), entry.getKey()));
        } else if (node.isArray()) {
            node.forEach(item -> refuseNonFinite(item, key));
        }
    }

    /** The core's JSON text as a {@code type}. {@code what} names the call, for the error. */
    static <T extends @Nullable Object> T decode(String text, TypeReference<T> type, String what) {
        try {
            return MAPPER.readValue(text, type);
        } catch (JsonProcessingException error) {
            throw unexpected(what, error);
        }
    }

    /** A part of the core's JSON as a {@code type}. */
    static <T> T convert(@Nullable JsonNode node, Class<T> type, String what) {
        try {
            return MAPPER.treeToValue(node, type);
        } catch (JsonProcessingException | IllegalArgumentException error) {
            throw unexpected(what, error);
        }
    }

    /** The core's JSON text as a tree. */
    static JsonNode tree(String text, String what) {
        try {
            return MAPPER.readTree(text);
        } catch (JsonProcessingException error) {
            throw unexpected(what, error);
        }
    }

    private static DataException unexpected(String what, Exception error) {
        String message = "the core's answer to " + what + " is not what this package expects: " + error.getMessage();
        return caused(new DataException(message, "ValueError"), error);
    }

    private static <E extends Throwable> E caused(E exception, Throwable cause) {
        exception.initCause(cause);
        return exception;
    }

    /** What a call of the core answered, or the exception for what it reported. */
    static String result(Lib.Outcome outcome) {
        if (!outcome.ok()) {
            throw error(outcome.json());
        }
        return outcome.json();
    }

    /** Runs a function of the core that needs no client and returns its JSON. */
    static String function(String name, Map<String, ? extends @Nullable Object> args) {
        return result(Lib.call(name, encode(args, null)));
    }

    private static @Nullable String text(JsonNode error, String field) {
        JsonNode value = error.path(field);
        return value.isTextual() ? value.textValue() : null;
    }

    /**
     * The exception for an error as the core reports it: the class of its {@code class}, or of
     * the nearest of its {@code bases} this package knows, or the root.
     */
    static OpenBydException error(String json) {
        JsonNode error;
        try {
            error = MAPPER.readTree(json);
        } catch (JsonProcessingException unreadable) {
            return new OpenBydException(json, "BydError");
        }
        String python = error.path("class").asText("BydError");
        ErrorDetails details =
                new ErrorDetails(python)
                        .code(text(error, "code"))
                        .endpoint(text(error, "endpoint"))
                        .errorType(text(error, "error_type"));
        if (error.path("status_code").isIntegralNumber()) {
            details.statusCode(error.path("status_code").longValue());
        }
        if (error.path("loc").isArray()) {
            List<String> loc = new ArrayList<>();
            error.path("loc").forEach(item -> loc.add(item.asText()));
            details.loc(loc);
        }
        BiFunction<String, ErrorDetails, OpenBydException> known = Errors.CLASSES.get(python);
        for (JsonNode base : error.path("bases")) {
            if (known != null) {
                break;
            }
            known = Errors.CLASSES.get(base.asText());
        }
        if (known == null) {
            known = OpenBydException::new;
        }
        return known.apply(error.path("message").asText(""), details);
    }

    /**
     * What a callback threw, as the core takes it back: an openbyd exception keeps its class and
     * fields, anything else is named by its own class.
     */
    static Map<String, @Nullable Object> errorToJson(Throwable error) {
        Map<String, @Nullable Object> json = new LinkedHashMap<>();
        String message = error.getMessage() != null ? error.getMessage() : "";
        if (!(error instanceof OpenBydException known)) {
            json.put("class", error.getClass().getSimpleName());
            json.put("message", message);
            return json;
        }
        json.put("class", known.kind());
        json.put("message", message);
        if (known instanceof BydApiException api) {
            json.put("code", api.code());
            json.put("endpoint", api.endpoint());
        } else if (known instanceof BydTransportException transport) {
            json.put("endpoint", transport.endpoint());
            json.put("status_code", transport.statusCode());
        }
        return json;
    }
}
