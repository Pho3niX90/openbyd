# Benchmarks

Two tools to notice a speed regression in `openbyd-protocol`.

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

The benchmark binary uses the system allocator, so its figures compare
with earlier runs; the Python extension and the `openbyd` binary use
mimalloc (cargo feature `mimalloc`, on by default), which roughly halves
the cost of the many small allocations a parse makes.

For the full realtime reply the by-reference `parse` + `dump` copies the
reply twice (into `raw`, and `raw` into the dump); `parse_owned` and
`into_dump` move it instead, which is about half the time. The bindings
use `parse_owned` and serialise the model straight into Python objects
(`crates/openbyd-py/src/dump.rs`), so there is no `Value` dump at all.

Through Python (`compare_pybyd.py`), what is left for the full realtime
reply is roughly a third each: converting the Python dict to a `Value`
(about 10 us), the parse, and building the Python dump, of which `raw`
alone is about 8 us (it could reuse the input's objects; not done).
