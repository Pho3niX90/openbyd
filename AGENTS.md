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
| `crates/openbyd-client` | The async client: HTTP, sessions, MQTT, commands. |
| `crates/openbyd-cli` | The `openbyd` command-line tool. |
| `crates/openbyd-py` | Python bindings, imported as `openbyd._native`. |
| `python/openbyd` | The Python package. |
| `python/tests` | Python tests, including the live comparison with pyBYD. |
| `parity/` | The parity framework, the oracles and the vector generator. |

Each file in `openbyd-protocol/src` names the pyBYD file it ports in its first doc line.

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

A model call takes `{"raw": ..., "context": ...}` and returns `{"dump": ..., "properties": ...}`: the `model_dump(mode="json")` and the value of every `@property` on the class. Functions that do I/O (`async def`) are ported in `openbyd-client` and covered by its scenario tests instead.

Rules for parity calls:

- **Errors.** When pyBYD raises, openbyd must fail with a `ParityError` whose `kind` is the Python exception's class name (`"BydCryptoError"`, `"ValidationError"`, `"ValueError"`, ...). Wording is not compared. A different class is a failure.
- **Outputs are compared exactly**, including `1` against `1.0`, key order where pyBYD's output is ordered, and `null` against a missing key.
- **Determinism.** Where pyBYD reads the clock or `secrets`, the oracle patches them and passes the same values to openbyd as arguments.
- **Cases must reach the awkward inputs:** empty values, missing keys, wrong types, sentinels (`""`, `"--"`, `-1`, `NaN`), numbers as strings, non-ASCII text, very large values. Fuzz sources should generate these on purpose, not only well-formed data.
- **Never weaken an oracle to make a test pass.** If pyBYD's behaviour looks like a bug, port the bug and note it in a comment. openbyd follows upstream.

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
cargo llvm-cov --workspace --exclude openbyd-py --summary-only   # line coverage
```

Do not use `maturin develop`: it writes the compiled module into `python/openbyd/`. Use `parity/dev_install.py`.

## Tests

- Everything is covered by tests. New code needs parity cases, and unit tests for anything parity cannot reach (error paths, argument checks, private helpers).
- Line coverage of `openbyd-crypto` and `openbyd-protocol` stays at or above 90%. CI enforces it.
- Tests never contact BYD. Client tests run against the fake server in the repo.
- Test data is synthetic. Never commit a real VIN, account name, token, location or plate. Use `TESTVIN0000000001`, `user@example.com` and coordinates such as `52.0, 4.0`.

## Code

- No `unsafe`. The workspace forbids it outside the Python bindings.
- Public items have a doc comment that says what the thing is for. Do not restate the code.
- Mirror pyBYD's names (`get_vehicle_realtime`, `VehicleRealtimeData`, field names in snake_case) so a reader can move between the two code bases.
- Keep `openbyd-protocol` free of I/O, async, the clock and randomness.
- A model serialises to exactly what pyBYD's `model_dump(mode="json")` gives for the same input. That JSON is the compatibility contract for every binding.
- Prefer a small helper to a macro, and a plain match to a framework.
- Commit messages say what changed and why, in plain prose. No tool or AI attribution lines.

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
4. Port until `cargo test` and `pytest` pass. Read the upstream commits (`git log <old>..<new>` in the reference checkout) to see intent, and port their tests too.
5. Anything that changes how a command is sent to a car gets a human review before release.
