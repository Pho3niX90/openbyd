using System.Collections;
using System.Globalization;
using System.Numerics;
using System.Reflection;
using System.Text.Json;
using OpenByd.Models;
using OpenByd.Testing;
using Xunit.Abstractions;

namespace OpenByd.Tests;

/// <summary>
/// The recorded pyBYD vectors, through this package.
///
/// <list type="bullet">
/// <item>Every vector gives pyBYD's answer through the C library, exactly.</item>
/// <item>Every model dump pyBYD produced is read by the record this package publishes for
/// it, strictly, and written back as the same JSON.</item>
/// <item><see cref="OpenBydCore.ParseModel{T}(string, string)"/> returns that dump.</item>
/// </list>
/// </summary>
public sealed class VectorsTests(ITestOutputHelper output)
{
    private sealed record Case(string File, int Index, string Call, JsonElement Args, JsonElement? Ok, string? Error);

    private static readonly JsonDocumentOptions Deep = new() { MaxDepth = 256 };

    private static readonly JsonSerializerOptions DeepSerializer = new() { MaxDepth = 256 };

    private static readonly string Root = FindRoot();

    private static readonly Lazy<(int Files, IReadOnlyList<Case> Cases)> Vectors = new(Load);

    /// <summary>The call name of each of pyBYD's reply models, with the record published for it.</summary>
    private static readonly Lazy<IReadOnlyDictionary<string, Type>> Models = new(() =>
    {
        using var surface = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(Root, "bindings", "surface.json")));
        var models = new Dictionary<string, Type>();
        foreach (var item in surface.RootElement.GetProperty("structs").EnumerateArray())
        {
            var (name, module) = (item.GetProperty("name").GetString()!, item.GetProperty("pybyd").GetString()!);
            if (module != "" && item.GetProperty("role").GetString() == "output")
            {
                var type = typeof(Client).Assembly.GetType($"OpenByd.Models.{name}");
                models[$"models.{module}.{name}"] = type ?? throw new InvalidOperationException($"no record {name}");
            }
        }
        return models;
    });

    private static string FindRoot()
    {
        for (var directory = new DirectoryInfo(AppContext.BaseDirectory); directory is not null; directory = directory.Parent)
        {
            if (File.Exists(Path.Combine(directory.FullName, "parity", "UPSTREAM_COMMIT")))
            {
                return directory.FullName;
            }
        }
        throw new InvalidOperationException("the tests run outside the repository");
    }

    private static (int Files, IReadOnlyList<Case> Cases) Load()
    {
        var cases = new List<Case>();
        var files = 0;
        foreach (var crate in new[] { "openbyd-protocol", "openbyd-client" })
        {
            var directory = Path.Combine(Root, "crates", crate, "tests", "vectors");
            foreach (var path in Directory.GetFiles(directory, "*.json").Order(StringComparer.Ordinal))
            {
                files += 1;
                // Kept for the life of the test run: the cases refer to the document.
                var document = JsonDocument.Parse(File.ReadAllBytes(path), Deep);
                if (!document.RootElement.TryGetProperty("cases", out var list))
                {
                    continue;
                }
                var index = 0;
                foreach (var item in list.EnumerateArray())
                {
                    cases.Add(new Case(
                        Path.GetFileName(path),
                        index++,
                        item.GetProperty("call").GetString()!,
                        item.GetProperty("args"),
                        item.TryGetProperty("ok", out var ok) ? ok : null,
                        item.TryGetProperty("error", out var error) ? error.GetString() : null));
                }
            }
        }
        return (files, cases);
    }

    // -- comparing JSON exactly ------------------------------------------------

    private static bool IsInteger(string number) => number.AsSpan().IndexOfAny('.', 'e', 'E') < 0;

    /// <summary>
    /// Whether two JSON numbers are the same: both integers or both floats (<c>1</c> is not
    /// <c>1.0</c>), with the same value however it is written (<c>1e+20</c> is <c>1E20</c>).
    /// </summary>
    private static bool SameNumber(string expected, string actual)
    {
        if (IsInteger(expected) != IsInteger(actual))
        {
            return false;
        }
        if (IsInteger(expected))
        {
            return BigInteger.Parse(expected, CultureInfo.InvariantCulture) == BigInteger.Parse(actual, CultureInfo.InvariantCulture);
        }
        var (left, right) = (double.Parse(expected, CultureInfo.InvariantCulture), double.Parse(actual, CultureInfo.InvariantCulture));
        return BitConverter.DoubleToInt64Bits(left) == BitConverter.DoubleToInt64Bits(right);
    }

    /// <summary>Where <paramref name="actual"/> differs from <paramref name="expected"/>, or null when it does not.</summary>
    private static string? Difference(JsonElement expected, JsonElement actual, string path = "$")
    {
        if (expected.ValueKind != actual.ValueKind)
        {
            return $"{path}: {actual.ValueKind} where {expected.ValueKind} was recorded";
        }
        switch (expected.ValueKind)
        {
            case JsonValueKind.Object:
            {
                // In order: pyBYD's dumps are ordered, and so is everything recorded from it.
                using var left = expected.EnumerateObject();
                using var right = actual.EnumerateObject();
                while (true)
                {
                    var (more, moreActual) = (left.MoveNext(), right.MoveNext());
                    if (more != moreActual)
                    {
                        return $"{path}: {(more ? $"lacks {left.Current.Name}" : $"has {right.Current.Name} too")}";
                    }
                    if (!more)
                    {
                        return null;
                    }
                    if (left.Current.Name != right.Current.Name)
                    {
                        return $"{path}: {right.Current.Name} where {left.Current.Name} was recorded";
                    }
                    if (Difference(left.Current.Value, right.Current.Value, $"{path}.{left.Current.Name}") is { } inner)
                    {
                        return inner;
                    }
                }
            }
            case JsonValueKind.Array:
            {
                if (expected.GetArrayLength() != actual.GetArrayLength())
                {
                    return $"{path}: {actual.GetArrayLength()} items where {expected.GetArrayLength()} were recorded";
                }
                var index = 0;
                foreach (var (left, right) in expected.EnumerateArray().Zip(actual.EnumerateArray()))
                {
                    if (Difference(left, right, $"{path}[{index++}]") is { } inner)
                    {
                        return inner;
                    }
                }
                return null;
            }
            case JsonValueKind.String:
                return expected.ValueEquals(actual.GetString()) ? null : $"{path}: {actual} where {expected} was recorded";
            case JsonValueKind.Number:
                return SameNumber(expected.GetRawText(), actual.GetRawText())
                    ? null
                    : $"{path}: {actual} where {expected} was recorded";
            default:
                return null;
        }
    }

    [Fact]
    public void TheComparisonIsExact()
    {
        static string? Compare(string expected, string actual)
        {
            using var left = JsonDocument.Parse(expected);
            using var right = JsonDocument.Parse(actual);
            return Difference(left.RootElement, right.RootElement);
        }

        Assert.Null(Compare("""{"a": [1, 1.0, 1e+20, "x", null, true]}""", """{"a":[1,1.0,1E20,"x",null,true]}"""));
        Assert.Null(Compare("0.1", "1e-1"));
        Assert.NotNull(Compare("1", "1.0"));
        Assert.NotNull(Compare("1.0", "1"));
        Assert.NotNull(Compare("0.1", "0.10000000000000002"));
        Assert.NotNull(Compare("0.0", "-0.0"));
        Assert.NotNull(Compare("12345678901234567890123", "12345678901234567890124"));
        Assert.NotNull(Compare("""{"a": 1, "b": 2}""", """{"b": 2, "a": 1}"""));
        Assert.NotNull(Compare("""{"a": 1}""", """{"a": 1, "b": 2}"""));
        Assert.NotNull(Compare("[1]", "[1, 2]"));
        Assert.NotNull(Compare("true", "false"));
        Assert.NotNull(Compare("null", "0"));
        Assert.NotNull(Compare("\"1\"", "1"));
    }

    [Fact]
    public void ThereAreVectorsToCheck()
    {
        Assert.True(Vectors.Value.Files >= 40, $"{Vectors.Value.Files} vector files");
    }

    [Fact]
    public void EveryVectorGivesPyBydsAnswer()
    {
        var cases = Vectors.Value.Cases;
        var problems = new List<string>();
        var ran = 0;
        foreach (var item in cases)
        {
            ran += 1;
            var got = Parity.Call(item.Call, item.Args.GetRawText());
            string? problem;
            if (item.Error is not null)
            {
                problem = got.ErrorKind == item.Error ? null : $"{got.ErrorKind ?? "an answer"} where {item.Error} was recorded";
            }
            else if (got.Ok is not { } ok)
            {
                problem = $"{got.ErrorKind}: {got.ErrorMessage}";
            }
            else
            {
                problem = Difference(item.Ok!.Value, ok);
            }
            if (problem is not null && problems.Count < 20)
            {
                problems.Add($"{item.File} #{item.Index} {item.Call}: {problem}");
            }
        }
        Assert.Empty(problems);
        // Unlike JavaScript, .NET carries every case: none is skipped.
        Assert.Equal(cases.Count, ran);
        Assert.True(ran > 32_000, $"{ran} vectors ran");
        output.WriteLine($"{ran} vectors ran through the C library");
    }

    /// <summary>How many floats JSON cannot hold a value has inside it.</summary>
    private static int NonFinite(object? value) => value switch
    {
        null or string or JsonElement => 0,
        double number => double.IsFinite(number) ? 0 : 1,
        IDictionary map => map.Values.Cast<object?>().Sum(NonFinite),
        IEnumerable items => items.Cast<object?>().Sum(NonFinite),
        _ when value.GetType().Namespace == "OpenByd.Models" && !value.GetType().IsEnum =>
            value.GetType().GetProperties(BindingFlags.Public | BindingFlags.Instance)
                .Sum(property => NonFinite(property.GetValue(value))),
        _ => 0,
    };

    [Fact]
    public void EveryModelDumpPyBydRecordedFitsItsRecord()
    {
        var models = Models.Value;
        var seen = new Dictionary<string, int>();
        var problems = new List<string>();
        var nonFinite = 0;
        foreach (var item in Vectors.Value.Cases)
        {
            if (!models.TryGetValue(item.Call, out var type) || item.Ok is not { } ok || !ok.TryGetProperty("dump", out var dump))
            {
                continue;
            }
            seen[item.Call] = seen.GetValueOrDefault(item.Call) + 1;
            string? problem;
            try
            {
                // Read strictly: every field, no other, no null where the type has none.
                var model = JsonSerializer.Deserialize(dump.GetRawText(), type, DeepSerializer);
                nonFinite += NonFinite(model);
                using var written = JsonDocument.Parse(JsonSerializer.Serialize(model, type, DeepSerializer), Deep);
                problem = Difference(dump, written.RootElement);
            }
            catch (JsonException error)
            {
                problem = error.Message;
            }
            if (problem is not null && problems.Count < 20)
            {
                problems.Add($"{item.File} #{item.Index} {item.Call}: {problem}");
            }
        }
        Assert.Empty(problems);
        // Every model has a recorded dump.
        Assert.DoesNotContain(models.Keys, call => !seen.ContainsKey(call));
        var total = seen.Values.Sum();
        Assert.True(total > 10_000, $"{total} dumps checked");
        Assert.True(nonFinite > 0, "no dump had a float JSON cannot hold");
        output.WriteLine($"{total} dumps of {seen.Count} models checked; {nonFinite} floats JSON cannot hold read back as numbers");
    }

    private static readonly MethodInfo ParseModelOfT = typeof(OpenBydCore).GetMethod(
        nameof(OpenBydCore.ParseModel), [typeof(JsonElement), typeof(JsonElement?)])!;

    [Fact]
    public void ParseModelReturnsPyBydsDump()
    {
        var models = Models.Value;
        var problems = new List<string>();
        var parsed = 0;
        var refused = 0;
        foreach (var item in Vectors.Value.Cases)
        {
            if (!models.TryGetValue(item.Call, out var type))
            {
                continue;
            }
            var raw = item.Args.GetProperty("raw");
            var context = item.Args.GetProperty("context");
            string? problem;
            try
            {
                var model = ParseModelOfT.MakeGenericMethod(type).Invoke(
                    null, [raw, context.ValueKind == JsonValueKind.Null ? null : (object)context]);
                using var written = JsonDocument.Parse(JsonSerializer.Serialize(model, type, DeepSerializer), Deep);
                problem = item.Ok is { } ok
                    ? Difference(ok.GetProperty("dump"), written.RootElement)
                    : $"an answer where {item.Error} was recorded";
                parsed += 1;
            }
            catch (TargetInvocationException thrown) when (thrown.InnerException is OpenBydException error)
            {
                problem = error.Kind == item.Error ? null : $"{error.Kind} where {item.Error ?? "an answer"} was recorded";
                refused += 1;
            }
            if (problem is not null && problems.Count < 20)
            {
                problems.Add($"{item.File} #{item.Index} {item.Call}: {problem}");
            }
        }
        Assert.Empty(problems);
        Assert.True(parsed > 10_000, $"{parsed} models parsed");
        Assert.True(refused > 0, "no vector was refused");
        output.WriteLine($"{parsed} models parsed, {refused} payloads refused as pyBYD refuses them");
    }
}
