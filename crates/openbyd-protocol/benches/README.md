# Benchmarks

Tools to notice a speed regression in `openbyd-protocol`, and to see where
the time of a reply goes.

## `protocol.rs`: the Rust hot paths

```sh
cargo bench -p openbyd-protocol --bench protocol            # everything
cargo bench -p openbyd-protocol --bench protocol -- realtime # names containing "realtime"
```

A plain `std::time::Instant` harness (no extra dependency, release profile).
Each line is one call: the median of 15 samples, with the fastest and slowest
sample in brackets. It measures, with the payloads in `parity/corpus/`:

| Line | What one call does |
|---|---|
| `realtime/full parse+dump (headline)` | `VehicleRealtimeData::parse` + `dump` of the "every field present" reply |
| `realtime/full parse_owned+into_dump` | the same through the by-value calls (`BydModel::parse_owned`, `into_dump`), plus cloning the input for them |
| `realtime/full clone` | that clone alone, to subtract |
| `realtime/full parse`, `dump`, `properties` | the three steps on their own |
| `realtime/corpus[i] parse+dump` | every realtime payload, with a HYBRID context |
| `model/<Class> parse+dump` | the largest corpus payload of each other model |
| `base/clean_byd_values`, `base/to_camel` | the public cleaning helpers (models no longer call `clean_byd_values`; `Fields::cleaned` does the same without copying) |
| `coerce/<kind> x6` | six awkward inputs through one conversion |
| `pyjson/...` | `dumps_compact` and `loads` of the full reply, `py_str` of a float |
| `envelope/build_token_outer_envelope` | encrypting, signing and checksumming a vehicle request |
| `aes/...` | encrypting an inner request, decrypting the full reply |
| `mqtt/decode_mqtt_payload` | decrypting and parsing a realtime push |
| `state/update_realtime` | one engine update with the full reply (includes cloning the model) |

Times swing by tens of percent when the machine is busy: compare runs made
back to back, and look at the minimum when the median is noisy.

## `allocations.rs`: allocations per call

```sh
cargo bench -p openbyd-protocol --bench allocations
```

How many heap blocks and bytes one call of the realtime rows allocates
(freed or not), counted by the `dhat` crate's allocator (a dev-dependency,
used only by this bench). The counts are exact and do not depend on the
machine, so a change that adds an allocation per field shows up at once;
the calls themselves run slowly under `dhat`, so this bench has no times.

## `compare_pybyd.py`: openbyd against pyBYD through Python

```sh
python parity/dev_install.py   # build the extension in release mode first
python crates/openbyd-protocol/benches/compare_pybyd.py [--iterations 20000] [--runs 5] [Class ...]
```

For each model with corpus payloads (its largest payload, and every realtime
payload) it checks that both libraries give the same dump, then times
`openbyd.models.parse(name, raw)` against pyBYD's
`Model.model_validate(raw).model_dump(mode="json")`, interleaved, and prints
the medians and pyBYD's time divided by openbyd's. That ratio is the number to
watch: absolute times move with the machine, the ratio much less.

## Where the time goes

The benchmark binaries use the system allocator, so their figures compare
with earlier runs; the Python extension and the `openbyd` binary use
mimalloc (cargo feature `mimalloc`, on by default), which roughly halves
the cost of the many small allocations a parse makes.

### One full realtime reply through Python

`openbyd.models.parse("VehicleRealtimeData", raw)` with the full synthetic
reply (141 keys), Windows 11, Python 3.12, mimalloc. The stage rows come
from a build with a timer around each stage and a counting allocator (the
local branch `perf/profile-harness`, not merged; `perf/profile-harness-after`
is the same profiler on the code after the two changes below). The stages
add up to the wall time of the instrumented loop within 0.1 us; that build
is about 3 us slower overall than a normal one.

| Stage | Before (us) | After (us) | Rust allocations before / after |
|---|---:|---:|---|
| input dict to `serde_json::Value` (`convert::to_value`) | 9.35 | 9.28 | 169 / 169 |
| `split_hybrid_legs` (the before-validator) | 2.63 | 2.51 | 38 / 38 |
| key table (`clean_table`) / key slots (`Fields::planned`) | 2.70 | 3.14 | 1 (6.4 KB) / 1 (2.2 KB) |
| 141 field reads (lookup and conversion) | 5.48 | 2.18 | 27 / 27 |
| everything else (call, GIL release, finish, sentinel rules, moving `raw`) | 0.48 | 0.56 | |
| dump of the fields (Python objects) | 6.52 | 6.33 | 6 / 6 |
| `raw` in the dump: from the parsed `Value` / copied from the caller's dict | 8.47 | 1.76 | 0 / 0 |
| dropping the model and the dump | 2.51 | 1.59 | |
| **Total (instrumented loop)** | **38.2** | **27.3** | |

On a normal build the whole call went from about 34 us to about 25 us
(`bench/profile_realtime.py`: 24.8 us minimum after); pyBYD's
`model_validate` plus `model_dump(mode="json")` takes about 85 us on the
same machine.

What changed:

- **`raw` from the caller's objects.** When a model's `raw` is the reply
  itself (`BydModel::parse_reply`), the bindings build the dump's `raw`
  from the caller's dict (`convert::round_tripped`: new containers, exact
  immutable leaves shared, the same result as the `Value` round trip)
  instead of converting the parsed `Value` back. The client's replies
  arrive as JSON text and still take the `Value` path.
- **Key plans.** A model's fields are read through its `Plan`
  (`models/base/plan.rs`): after the first successful parse, one pass over
  the reply fills a slot per key a field can ask for, and each field read
  is an array read instead of hashing the key and its camelCase alias.

### What is left

- **Input conversion, about 9 us** of the 25: walking the dict (about
  2.7 us), a `String` per key (1.4 us), the leaf values (1.4 us) and the
  `serde_json::Map` inserts, each key hashed with SipHash (about 4 us).
  Removing it needs models to parse from something other than a
  `serde_json::Value` (the Python objects themselves, or slots filled
  straight from the dict), which changes how every model reads its input.
- The dump of the fields (about 6 us: a Python object per value and a dict
  insert per field) and the before-validator (about 2.5 us).
- In the client, the model is a small part of a call: one
  `get_vehicle_realtime` against the fake server spends far more on the
  envelope cipher (about 0.4 ms to decode the realtime reply) and the
  round trips than on the model.

### Reproducing

```sh
python bench/profile_realtime.py               # the rows above that need no instrumented build
python bench/profile_realtime.py --whole-call  # and one Client.get_vehicle_realtime, split up
cargo bench -p openbyd-protocol --bench allocations
```

`profile_realtime.py` times `openbyd.models.parse`, the input conversion
alone (`_Value(raw)`), building `raw` from a `Value` (`_Value.to_python()`,
which the client still does), and pyBYD's validate, dump and dump without
`raw`. The per-stage table needs the instrumented build: check out
`perf/profile-harness-after`, build the extension with
`--features testing,profile` (`maturin build`, then install the wheel) and
run `prof_py.py` (kept on `perf/profile-harness` as
`crates/openbyd-perf/prof_py.py`; a scratch script).
