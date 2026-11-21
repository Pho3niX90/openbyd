package io.github.pho3nix90.openbyd;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.json.JsonMapper;
import io.github.pho3nix90.openbyd.testing.Parity;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.stream.Stream;
import org.jspecify.annotations.Nullable;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

/**
 * The recorded pyBYD vectors, through this package.
 *
 * <ul>
 *   <li>Every vector gives pyBYD's answer through the native library.
 *   <li>Every model dump pyBYD recorded reads into the record generated for it, strictly, and
 *       writes back as the same JSON.
 *   <li>{@code OpenByd.parseModel} returns that dump.
 * </ul>
 *
 * <p>JSON is compared exactly: a number by its kind (an integer is not a float, so {@code 1} is
 * not {@code 1.0}) and its value, an object with its keys in order.
 */
class VectorsTest {
    /**
     * Reads JSON without losing anything: an integer of any size is exact, and a float is the
     * double it is in Python, which wrote the vectors (so {@code -0.0} keeps its sign, and how
     * the text spells a float, {@code 1e+20} or {@code 1.0E20}, does not matter).
     */
    private static final ObjectMapper EXACT =
            JsonMapper.builder().enable(DeserializationFeature.USE_BIG_INTEGER_FOR_INTS).build();

    private static final TypeReference<Map<String, @Nullable Object>> OBJECT = new TypeReference<>() {};

    private record Case(String where, String call, JsonNode args, @Nullable JsonNode ok, @Nullable String error) {}

    private static final List<Case> CASES = new ArrayList<>();
    private static int files;

    @BeforeAll
    static void load() throws IOException {
        Path root = Path.of(System.getProperty("openbyd.repository"));
        for (String crate : List.of("openbyd-protocol", "openbyd-client")) {
            Path directory = root.resolve("crates").resolve(crate).resolve("tests").resolve("vectors");
            List<Path> paths;
            try (Stream<Path> listed = Files.list(directory)) {
                paths = listed.filter(path -> path.toString().endsWith(".json")).sorted().toList();
            }
            for (Path path : paths) {
                JsonNode cases = EXACT.readTree(path.toFile()).get("cases");
                if (cases == null) {
                    continue;
                }
                files += 1;
                int index = 0;
                for (JsonNode item : cases) {
                    String where = crate + "/" + path.getFileName() + " #" + index++;
                    JsonNode error = item.get("error");
                    CASES.add(
                            new Case(
                                    where,
                                    item.get("call").textValue(),
                                    item.get("args"),
                                    item.get("ok"),
                                    error == null ? null : error.textValue()));
                }
            }
        }
    }

    /** Where two JSON values differ, or null when they are the same, exactly. */
    static @Nullable String difference(JsonNode expected, JsonNode got, String path) {
        if (expected.isNumber() && got.isNumber()) {
            if (expected.isIntegralNumber() != got.isIntegralNumber()) {
                return path + ": " + expected + " and " + got + " are not the same kind of number";
            }
            boolean same =
                    expected.isIntegralNumber()
                            ? expected.bigIntegerValue().equals(got.bigIntegerValue())
                            : Double.doubleToLongBits(expected.doubleValue()) == Double.doubleToLongBits(got.doubleValue());
            return same ? null : path + ": expected " + expected + ", got " + got;
        }
        if (expected.getNodeType() != got.getNodeType()) {
            return path + ": expected " + expected + ", got " + got;
        }
        if (expected.isObject()) {
            Iterator<String> names = expected.fieldNames();
            Iterator<String> others = got.fieldNames();
            while (names.hasNext() && others.hasNext()) {
                String name = names.next();
                String other = others.next();
                if (!name.equals(other)) {
                    return path + ": expected the key " + name + ", got " + other;
                }
                String inside = difference(expected.get(name), got.get(name), path + "." + name);
                if (inside != null) {
                    return inside;
                }
            }
            return names.hasNext() || others.hasNext() ? path + ": one has more keys" : null;
        }
        if (expected.isArray()) {
            if (expected.size() != got.size()) {
                return path + ": expected " + expected.size() + " items, got " + got.size();
            }
            for (int index = 0; index < expected.size(); index++) {
                String inside = difference(expected.get(index), got.get(index), path + "[" + index + "]");
                if (inside != null) {
                    return inside;
                }
            }
            return null;
        }
        return expected.equals(got) ? null : path + ": expected " + expected + ", got " + got;
    }

    private static void note(List<String> problems, Case item, String problem) {
        if (problems.size() < 20) {
            problems.add(item.where() + " " + item.call() + ": " + problem);
        }
    }

    @Test
    void thereAreVectorsToCheck() {
        assertTrue(files >= 40, files + " vector files");
        assertTrue(CASES.size() > 32_000, CASES.size() + " cases");
    }

    @Test
    void exactComparisonTellsNumbersApart() throws JsonProcessingException {
        JsonNode one = EXACT.readTree("[1, 1.0, 1e+20, -0.0, 0.0, 123456789012345678901234567890, {\"a\": 1, \"b\": 2}]");
        JsonNode same = EXACT.readTree(EXACT.writeValueAsString(one));
        assertNull(difference(one, same, ""));
        assertNull(difference(EXACT.readTree("1e+20"), EXACT.readTree("1.0E20"), ""));
        assertNull(difference(EXACT.readTree("100000000000000000000.0"), EXACT.readTree("1e20"), ""));
        for (String[] pair :
                new String[][] {
                    {"1", "1.0"},
                    {"0.0", "-0.0"},
                    {"123456789012345678901234567890", "123456789012345678901234567891"},
                    {"100000000000000000000", "1e20"},
                    {"{\"a\": 1, \"b\": 2}", "{\"b\": 2, \"a\": 1}"},
                    {"[1, 2]", "[1, 2, 3]"},
                    {"\"1\"", "1"},
                    {"null", "0"},
                }) {
            assertTrue(difference(EXACT.readTree(pair[0]), EXACT.readTree(pair[1]), "") != null, pair[0] + " " + pair[1]);
        }
    }

    @Test
    void everyVectorGivesPybydsAnswer() throws JsonProcessingException {
        int ran = 0;
        List<String> problems = new ArrayList<>();
        for (Case item : CASES) {
            // The arguments go as text: nothing between the vector and the core rounds a number.
            JsonNode got = EXACT.readTree(Parity.call(item.call(), EXACT.writeValueAsString(item.args())));
            ran += 1;
            JsonNode failure = got.get("error");
            if (item.error() != null) {
                if (failure == null || !item.error().equals(failure.path("kind").asText())) {
                    note(problems, item, "expected the error " + item.error() + ", got " + got);
                }
            } else if (failure != null) {
                note(problems, item, "got the error " + failure);
            } else {
                String differs = difference(item.ok(), got.get("ok"), "ok");
                if (differs != null) {
                    note(problems, item, differs);
                }
            }
        }
        assertEquals(List.of(), problems);
        assertEquals(CASES.size(), ran);
        System.out.println(ran + " vectors ran through the native library");
    }

    /** The record of each pyBYD model, by the core's call name. */
    private static Map<String, Class<?>> models() {
        Map<String, Class<?>> models = new LinkedHashMap<>();
        Models.CALLS.forEach((model, call) -> models.put(call, model));
        return models;
    }

    @Test
    void everyModelDumpReadsIntoItsRecordAndWritesBackTheSame() throws JsonProcessingException {
        Map<String, Class<?>> models = models();
        Map<String, Integer> seen = new TreeMap<>();
        int nonFinite = 0;
        List<String> problems = new ArrayList<>();
        for (Case item : CASES) {
            Class<?> model = models.get(item.call());
            JsonNode dump = item.ok() == null ? null : item.ok().get("dump");
            if (model == null || dump == null) {
                continue;
            }
            seen.merge(item.call(), 1, Integer::sum);
            String text = EXACT.writeValueAsString(dump);
            if (text.contains("\"NaN\"") || text.contains("Infinity\"")) {
                nonFinite += 1;
            }
            String back;
            try {
                back = Json.MAPPER.writeValueAsString(Json.MAPPER.readValue(text, model));
            } catch (JsonProcessingException refused) {
                note(problems, item, "does not read as " + model.getSimpleName() + ": " + refused.getOriginalMessage());
                continue;
            }
            String differs = difference(dump, EXACT.readTree(back), model.getSimpleName());
            if (differs != null) {
                note(problems, item, differs);
            }
        }
        assertEquals(List.of(), problems);
        List<String> unchecked = models.keySet().stream().filter(call -> !seen.containsKey(call)).toList();
        assertEquals(List.of(), unchecked, "models without a recorded dump");
        int total = seen.values().stream().mapToInt(Integer::intValue).sum();
        assertTrue(total > 10_000, total + " dumps checked");
        assertTrue(nonFinite > 0, "no dump had a float JSON cannot hold");
        System.out.println(total + " dumps of " + seen.size() + " models read into their records and written back");
    }

    @Test
    void parseModelReturnsPybydsDump() throws JsonProcessingException {
        Map<String, Class<?>> models = models();
        int parsed = 0;
        int refused = 0;
        List<String> problems = new ArrayList<>();
        for (Case item : CASES) {
            Class<?> model = models.get(item.call());
            JsonNode raw = item.args().get("raw");
            JsonNode context = item.args().get("context");
            if (model == null || raw == null || !raw.isObject() || (context != null && !context.isObject() && !context.isNull())) {
                continue;
            }
            Map<String, @Nullable Object> rawFields = EXACT.convertValue(raw, OBJECT);
            Map<String, @Nullable Object> contextFields =
                    context == null || context.isNull() ? null : EXACT.convertValue(context, OBJECT);
            Object record;
            try {
                record = OpenByd.parseModel(model, rawFields, contextFields);
            } catch (OpenBydException error) {
                refused += 1;
                if (!error.kind().equals(item.error())) {
                    note(problems, item, "expected " + (item.error() == null ? "a dump" : item.error()) + ", got " + error);
                }
                continue;
            }
            if (item.ok() == null) {
                note(problems, item, "expected the error " + item.error());
                continue;
            }
            parsed += 1;
            JsonNode back = EXACT.readTree(Json.MAPPER.writeValueAsString(record));
            String differs = difference(item.ok().get("dump"), back, model.getSimpleName());
            if (differs != null) {
                note(problems, item, differs);
            }
        }
        assertEquals(List.of(), problems);
        assertTrue(parsed > 10_000, parsed + " models parsed");
        assertTrue(refused > 0, "no payload was refused");
        System.out.println(parsed + " payloads parsed as their models, " + refused + " refused as pyBYD refuses them");
    }
}
