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

For the full realtime reply most of what is left is memory allocation: the
API returns `serde_json::Value`s, so a parse copies the reply into `raw`
(about 170 allocations) and a dump allocates every field name and copies
`raw` again (about 350). On Windows' default heap that is roughly half of
the parse+dump time. Through Python, converting the reply to a `Value` and
the dump back to Python objects (in `openbyd-py`) costs about as much again.
