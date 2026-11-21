# Working on openbyd

openbyd is a Rust port of [pyBYD](https://github.com/jkaberg/pyBYD), an unofficial client for the BYD overseas app API. pyBYD is the reference: openbyd must behave the same, and every ported piece is tested against it.

Read this file before changing anything. It applies to people and to coding agents.

## The reference

`parity/UPSTREAM_COMMIT` names the pyBYD commit openbyd matches. Port from that commit, not from a newer or older one. Check it out next to this repo and read the Python source directly:

```sh
git clone https://github.com/jkaberg/pyBYD ../pybyd-ref
git -C ../pybyd-ref checkout "$(cat parity/UPSTREAM_COMMIT)"
```

The same commit must be installed in the Python environment you test with.

## Layout

| Path | What it is |
|---|---|
| `crates/openbyd-crypto` | The envelope cipher. No dependencies. |
| `crates/openbyd-protocol` | Signing, envelopes, models, error codes, request building. No network, no clock, no randomness. |
| `crates/openbyd-client` | The async client. `api/` ports pyBYD's `_api` flows, generic over the `Env` trait (`env.rs`); `scripted.rs` is the scripted `Env` the parity tests run them in. HTTP, sessions and MQTT come later. |
| `crates/openbyd-cli` | The `openbyd` command-line tool. |
| `crates/openbyd-py` | Python bindings, imported as `openbyd._native`. |
| `python/openbyd` | The Python package. |
| `crates/openbyd-host` | The client by method name, with JSON in and out: the one layer the other languages' bindings share. |
| `crates/openbyd-node`, `crates/openbyd-capi` | `openbyd-host` as a Node add-on, and as a C library (`include/openbyd.h`) for the JVM and .NET. |
| `bindings/` | The TypeScript, Java and Kotlin, and C# packages (`node/`, `jvm/`, `dotnet/`), and what generates their typed layers: `spec.py`, `generate.py`, `gen/`. See [`bindings/README.md`](bindings/README.md). |
| `python/tests` | Python tests, including the live comparison with pyBYD. |
| `parity/` | The parity framework, the oracles and the vector generator. `api_support.py` runs pyBYD's flows against a script. |
| `crates/*/tests/vectors` | Recorded pyBYD answers: `openbyd-protocol`'s for pure calls, `openbyd-client`'s for flows. |

Each file in `openbyd-protocol/src` and `openbyd-client/src/api` names the pyBYD file it ports in its first doc line.

## The parity rule

**Every function ported from pyBYD is reachable by a call name and compared with pyBYD.** A port without this is not done.

To port a function:

1. Write the Rust function in the module that mirrors the pyBYD file. It takes the time and any random value as arguments; `openbyd-protocol` never reads the clock or a random source.
2. Add an arm to that module's `parity_call` under the name `"<module>.<function>"`. It takes its arguments as a JSON object and returns JSON. See `hashing.rs`.
3. In `parity/groups/<module>.py`, register an oracle for the name that calls the pyBYD function, a `recorded` source of fixed cases and a `fuzz` source of random ones. See `parity/groups/hashing.py` and the docstring of `parity/framework.py`.
4. Run `python parity/generate.py <group>` to record pyBYD's answers.
5. Run the tests below until the Rust tests and the Python tests both pass.

Call names follow the pyBYD file, so the surface audit can work out which ones should exist:

| pyBYD | Call name |
|---|---|
| `_crypto/hashing.py`, `aes.py`, `signing.py` | `hashing.<function>`, `aes.<function>`, `signing.<function>` |
| `_constants.py`, `_redact.py`, `_mqtt.py` | `constants.<function>`, `redact.<function>`, `mqtt.<function>` |
| `config.py`, `session.py` | `config.<Class>`, `session.<method>` |
| `_api/login.py`, `_api/_envelope.py`, `_transport.py` | `envelope.<function>` |
| `_api/_common.py` | `api.common.<function>` |
| `_api/<file>.py` | `api.<file>.<function>` |
| `models/_base.py` | `models.base.<function>` |
| `models/<file>.py` | `models.<file>.<Class>` for a model or enum, `models.<file>.<function>` for a function |
| `_capabilities/<file>.py` | `capabilities.<file>.<name>` |
| `_state_engine.py`, `_validators.py` | `state.<name>` |

A model call takes `{"raw": ..., "context": ...}` and returns `{"dump": ..., "properties": ...}`: the `model_dump(mode="json")` and the value of every `@property` on the class. Functions that do I/O (`async def`) are ported in `openbyd-client` under the same call names and compared by trace: see [Porting an `_api` function](#porting-an-_api-function). The client crate answers the names the protocol crate does not know.

Rules for parity calls:

- **Errors.** When pyBYD raises, openbyd must fail with a `ParityError` whose `kind` is the Python exception's class name (`"BydCryptoError"`, `"ValidationError"`, `"ValueError"`, ...). Wording is not compared. A different class is a failure.
- **Outputs are compared exactly**, including `1` against `1.0`, key order where pyBYD's output is ordered, and `null` against a missing key.
- **Determinism.** Where pyBYD reads the clock or `secrets`, the oracle patches them and passes the same values to openbyd as arguments.
- **Cases must reach the awkward inputs:** empty values, missing keys, wrong types, sentinels (`""`, `"--"`, `-1`, `NaN`), numbers as strings, non-ASCII text, very large values. Fuzz sources should generate these on purpose, not only well-formed data.
- **Never weaken an oracle to make a test pass.** If pyBYD's behaviour looks like a bug, port the bug and note it in a comment. openbyd follows upstream.

## Porting an `_api` function

pyBYD's `_api/<file>.py` functions are `async def`s that build a request, post it, and interpret the reply; some loop with sleeps and retries. Each is ported 1:1 as an `async fn` in `crates/openbyd-client/src/api/<file>.rs` with the same name, generic over `Env`:

```rust
pub async fn fetch_gps_endpoint<E: Env>(
    cx: Context<'_, E>,          // pyBYD's config, session and transport
    endpoint: &str,
    vin: &str,
    request_serial: Option<&str>,
) -> Result<(Value, Option<String>), Error>
```

`Env` (`env.rs`) is everything a flow may touch: `now_ms()` (`int(time.time() * 1000)`), `monotonic()`, `random_bytes()` (`secrets.token_hex(16)`), `sleep(seconds)` and `post_secure(endpoint, outer)` (`SecureTransport.post_secure`). Read the clock and draw random values where pyBYD does: call `env.now_ms()` wherever pyBYD calls `time.time()`. The session is shared by reference; change its guest state only with `Session::set_guest` and `Session::settle_user_type`. Request building, signing and code mapping are pure and live in `openbyd-protocol` (`build_inner_base`, `build_token_outer_envelope`, `handle_token_response`, `raise_for_code`); reuse them, and call `api::common::post_token_json` / `post_vehicle_json` where pyBYD does.

Steps:

1. Write the function in `api/<file>.rs`, replacing the `no_calls!()` stub.
2. Add an arm to that module's `parity_call` that builds a `Scenario` from the arguments, reads the function's own arguments with `scenario.args()` and returns `scenario.run(flow, to_json)`. See `api/gps.rs`.
3. In `parity/groups/api_<file>.py`, create `group = Group("api_<file>", crate="client")` and register the oracle in one line: `flow_oracle(group, "api.<file>.<function>", <pybyd function>)`. Pure helpers of the same file get ordinary oracles.
4. Add recorded scripts and a fuzz source with the builders in `parity/api_support.py`: `scenario(...)`, `ok_reply(session, data)`, `error_reply(code)`, `sealed_reply`, `raw_reply`, `transport_error(status)`. `vehicle_scripts(endpoint, ok)` walks every branch of the guest retry; `random_vehicle_script` and `random_reply` vary replies (success, every code, wrong shapes, non-object replies, transport errors, guest and owner). Cover every code the function maps and every way out of its loops. See `parity/groups/api_gps.py`.
5. `python parity/generate.py api_<file>`, then the tests as usual. The vectors land in `crates/openbyd-client/tests/vectors/`.

A flow's parity call takes `config` and `session` (keyword arguments; give `created_at`), `guest_vins` and `user_type_settled` (the session's guest state before the call), `now_ms`, `seed`, `replies` (the script) and `args` (the function's own arguments; a list for a code set, `[[codes], "ClassName"]` pairs for an `extra_code_map`). It returns the trace:

```json
{
  "steps": [{"post": "/control/getGpsInfo", "outer": {"...": "the complete signed outer payload"}}, {"sleep": 2.0}],
  "result": ["the return value as JSON"],
  "guest_vins": {"TESTVIN0000000001": true},
  "user_type_settled": [["TESTVIN0000000001", "/control/getGpsInfo"]],
  "unused_replies": 0
}
```

On failure `"error": {"class", "code", "endpoint", "status_code"}` replaces `"result"` (`null` where the class has no such attribute; the message is not compared). pyBYD's side runs the real function with `transport.post_secure` replaced by the script and `time.time`, `time.monotonic`, `secrets.token_hex` and `asyncio.sleep` patched, without an event loop. Both sides follow the same rules:

- the wall clock is `now_ms` plus the total time slept so far (`now_ms + int(slept * 1000)`; Python's `time.time()` returns it plus half a millisecond, so `int(time.time() * 1000)` is exact); the monotonic clock is `now_ms / 1000 + slept`;
- the n-th random value (from 0) is `MD5("<seed>:<n>")`;
- a reply is `{"reply": <the decoded outer reply>}` or `{"transport_error": {"status_code": ...}}`; `ok_reply` encrypts `respondData` with the session's content key when the case is built, so vectors are self-contained;
- a request with no reply left fails with `ScriptExhausted`, which no pyBYD `except` catches;
- a flow that waits on anything else (a real future) is a test bug, reported as `Stalled`.

Debugging a trace mismatch: the test prints both traces. Compare `steps` first. A different number of posts or sleeps means a branch differs. The same steps with a different `encryData`, `sign` or `checkcode` mean the inner payload differs: its key order, or a clock read or random draw in a different place (`reqTimestamp`, `serviceTime` and the inner `random` and `timeStamp` show which). Then compare `error`, and last `guest_vins` and `user_type_settled`. To look at one case, call `parity.api_support.run_flow(<pybyd function>, **args)` and `openbyd._native.parity_call(name, json.dumps(args))` with the printed arguments.

`_mqtt.fetch_mqtt_bootstrap` is an `async def` too: port it in the client crate as `mqtt.fetch_mqtt_bootstrap` the same way.

## Commands

Set up once (Python 3.12, any virtual environment):

```sh
pip install maturin pytest "pybyd @ git+https://github.com/jkaberg/pyBYD@$(cat parity/UPSTREAM_COMMIT)"
```

Then:

```sh
cargo test --workspace --exclude openbyd-py   # Rust unit tests and recorded vectors
python parity/dev_install.py                  # build the extension into the current environment
python parity/generate.py <group>             # re-record a group's vectors from pyBYD
python parity/generate.py --check             # fail if any vector file is out of date
pytest                                        # Python tests, including random comparison with pyBYD
OPENBYD_FUZZ=5000 OPENBYD_PARITY_SEED=x pytest python/tests/test_parity.py   # a longer, different random run
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo llvm-cov --workspace --exclude openbyd-py --exclude openbyd-node --summary-only   # line coverage
python bindings/generate.py                   # write the bindings' typed layers (needs the pinned pyBYD)
python bindings/generate.py --check           # fail if any generated file is out of date
```

Each binding's own build and tests are in [`bindings/README.md`](bindings/README.md).

Do not use `maturin develop`: it writes the compiled module into `python/openbyd/`. Use `parity/dev_install.py`.

## Tests

- Everything is covered by tests. New code needs parity cases, and unit tests for anything parity cannot reach (error paths, argument checks, private helpers).
- Line coverage of `openbyd-crypto` and `openbyd-protocol` stays at or above 90%. CI enforces it. Keep `openbyd-client` there too.
- Tests never contact BYD. Client flows run against the scripted environment (`openbyd_client::scripted`).
- Test data is synthetic. Never commit a real VIN, account name, token, location or plate. Use `TESTVIN0000000001`, `user@example.com` and coordinates such as `52.0, 4.0`.

## Code

- No `unsafe`. The workspace forbids it outside the crates that cross into another language (`openbyd-py`, `openbyd-node`, `openbyd-capi`).
- Public items have a doc comment that says what the thing is for. Do not restate the code.
- Mirror pyBYD's names (`get_vehicle_realtime`, `VehicleRealtimeData`, field names in snake_case) so a reader can move between the two code bases.
- Keep `openbyd-protocol` free of I/O, async, the clock and randomness. In `openbyd-client`, flows reach them only through `Env`.
- A model serialises to exactly what pyBYD's `model_dump(mode="json")` gives for the same input. That JSON is the compatibility contract for every binding.
- Prefer a small helper to a macro, and a plain match to a framework.
- Commit messages say what changed and why, in plain prose. No tool or AI attribution lines.

## The other languages' bindings

TypeScript, Java and Kotlin, and C# share one layer, `openbyd-host`, and each has a typed layer generated from `bindings/spec.py` and from pyBYD's own models. [`bindings/README.md`](bindings/README.md) describes it. The rules:

- **Nothing language-specific goes into `openbyd-host`, and no client logic goes into a binding.** A binding loads the native library, names methods, converts JSON to its types and runs callbacks on its own threads. If two bindings need the same behaviour, it belongs in `openbyd-host`.
- **A new or changed method is three steps:** the arm in `crates/openbyd-host/src/client.rs` (and its name in `ASYNC_METHODS` or `SYNC_METHODS`), its description in `bindings/spec.py`, then `python bindings/generate.py`. `cargo test -p openbyd-host --all-features` fails when the two disagree.
- **Never edit a generated file** (`bindings/node/src/generated`, `bindings/jvm/src/generated`, `bindings/dotnet/OpenByd/Generated`). Change the spec or the emitter in `bindings/gen/` and regenerate. CI runs `generate.py --check`.
- **The shapes of pyBYD's models are never written by hand.** They are read from the pyBYD installed, so the published types move with `parity/UPSTREAM_COMMIT`.
- **Every package keeps its three kinds of test:** the client end to end against the fake server and the test broker (`openbyd_host::testing`), the recorded vectors through its native library, and every recorded pyBYD model dump read into its types.
- **A package's version is the workspace's.** `generate.py` fails when `package.json`, `pom.xml` or `OpenByd.csproj` says otherwise.

## Several people in one checkout

When more than one person or agent works in the same checkout at once:

- **Own your files.** Edit only the files assigned to you. Shared files (`lib.rs`, `mod.rs`, `parity.rs`, the workspace `Cargo.toml`, `parity/framework.py`) are changed only by the person coordinating.
- **Format only your own files:** `rustfmt --edition 2024 <your files>`. `cargo fmt --all` rewrites files someone else is editing.
- **Record only your own groups:** always pass group names to `parity/generate.py`.
- **Use your own build directory and environment**, so builds do not block each other:
  `CARGO_TARGET_DIR=target/agents/<name>` and a virtual environment at `.venv-<name>`.
- **Add a dependency** with `cargo add -p <crate> <dependency>` and say so in your hand-over.
- **Do not run git commands that change anything** (`add`, `commit`, `stash`, `checkout`, `reset`). The coordinator commits.

## Following upstream

When pyBYD moves:

1. Put the new commit in `parity/UPSTREAM_COMMIT` and install it.
2. Run `python parity/generate.py`. Changed vector files show what upstream changed in existing behaviour.
3. Run `pytest python/tests/test_surface.py`. It fails for every public pyBYD model, endpoint or client method that has no parity entry yet.
4. Run `python bindings/generate.py`. The other languages' types follow the new models; a new client method also needs its lines in `openbyd-host` and `bindings/spec.py`.
5. Port until `cargo test`, `pytest` and the bindings' tests pass. Read the upstream commits (`git log <old>..<new>` in the reference checkout) to see intent, and port their tests too.
6. Anything that changes how a command is sent to a car gets a human review before release.
