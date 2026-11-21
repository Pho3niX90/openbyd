package io.github.pho3nix90.openbyd;

import com.fasterxml.jackson.databind.JsonNode;
import java.util.HexFormat;
import java.util.Map;
import org.jspecify.annotations.Nullable;

/** What needs no client: the envelope cipher, pyBYD's models by class, the runtime's settings. */
public final class OpenByd {
    private OpenByd() {}

    /** The version of openbyd's core, which is this package's. */
    public static String version() {
        return Json.tree(Json.function("version", Map.of()), "version").asText();
    }

    /** Wraps text in a Bangcle envelope, as BYD's app does with every request. */
    public static String encodeEnvelope(String plaintext) {
        return Json.tree(Json.function("encode_envelope", Map.of("text", plaintext)), "encode_envelope").asText();
    }

    /** Wraps bytes in a Bangcle envelope. */
    public static String encodeEnvelope(byte[] plaintext) {
        Map<String, String> args = Map.of("hex", HexFormat.of().formatHex(plaintext));
        return Json.tree(Json.function("encode_envelope", args), "encode_envelope").asText();
    }

    /**
     * Opens a Bangcle envelope and returns the plaintext bytes.
     *
     * @throws BangcleException when the envelope is a bad one
     */
    public static byte[] decodeEnvelope(String envelope) {
        JsonNode plain = Json.tree(Json.function("decode_envelope", Map.of("envelope", envelope)), "decode_envelope");
        return HexFormat.of().parseHex(plain.path("hex").asText());
    }

    /**
     * Parses a raw payload (a reply's {@code respondData}, an MQTT event's data) as one of
     * pyBYD's reply models: {@code Model.model_validate(raw)}.
     *
     * @param model The model's record, such as {@code GpsInfo.class}.
     * @param raw The payload.
     * @throws ValidationException when the payload does not validate
     * @throws IllegalArgumentException when {@code model} is not one of pyBYD's reply models
     */
    public static <T> T parseModel(Class<T> model, Map<String, ? extends @Nullable Object> raw) {
        return parseModel(model, raw, null);
    }

    /**
     * Parses a raw payload as one of pyBYD's reply models, with pyBYD's validation context for
     * the models that take one.
     *
     * @param model The model's record, such as {@code VehicleRealtimeData.class}.
     * @param raw The payload.
     * @param context pyBYD's validation context, or null for none.
     * @throws ValidationException when the payload does not validate
     * @throws IllegalArgumentException when {@code model} is not one of pyBYD's reply models
     */
    public static <T> T parseModel(
            Class<T> model,
            Map<String, ? extends @Nullable Object> raw,
            @Nullable Map<String, ? extends @Nullable Object> context) {
        String name = Models.CALLS.get(model);
        if (name == null) {
            throw new IllegalArgumentException(model.getName() + " is not one of pyBYD's reply models");
        }
        String parsed = Json.function("parse_model", Invoker.args("name", name, "raw", raw, "context", context));
        return Json.convert(Json.tree(parsed, "parse_model").get("dump"), model, "parse_model");
    }

    /** The state of the Rust runtime every client shares. */
    public static RuntimeInfo runtimeInfo() {
        return Json.convert(Json.tree(Json.function("runtime", Map.of()), "runtime"), RuntimeInfo.class, "runtime");
    }

    /**
     * Sets the worker threads of the Rust runtime (2 by default). Only before the runtime
     * starts, which the first client does.
     *
     * @throws DataException when the runtime has started, or the number cannot be used
     */
    public static void configureRuntime(int workerThreads) {
        Json.function("configure", Map.of("worker_threads", workerThreads));
    }
}
