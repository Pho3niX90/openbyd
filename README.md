# openbyd

An unofficial client for the BYD overseas app API, written in Rust, with Python bindings.

openbyd is a port of [pyBYD](https://github.com/jkaberg/pyBYD). pyBYD stays the reference: every piece ported here is tested against it, and the pyBYD commit openbyd currently matches is recorded in `parity/UPSTREAM_COMMIT`.

This project is not affiliated with, endorsed by or supported by BYD.

## Status

All of pyBYD is ported and compared with pyBYD in the tests. **It has not yet been run against BYD's live service**, so treat it as pre-release, above all for commands that act on a car.

| Part | Crate or module | State |
|---|---|---|
| Envelope cipher | `openbyd-crypto` | Matches pyBYD byte for byte |
| Signing, envelopes, reply models, error codes, command gating, state engine | `openbyd-protocol` | Ported; compared with pyBYD on recorded and random inputs |
| Async client: sessions, reads, commands, MQTT, the car layer | `openbyd-client` | Ported; every request sequence compared with pyBYD's against a script, plus tests over real sockets with a fake server and broker |
| Python package | `openbyd` | `openbyd.Client` and `openbyd.Car` with pyBYD's method names, returning plain dicts |
| Command-line tool | `openbyd-cli` | `openbyd login`, `vehicles`, `dump`, `get`, `watch`, `command` ([usage](crates/openbyd-cli/README.md)) |
| Drop-in speed-up for pyBYD | `openbyd.pybyd_accel` | See below |

An audit in the test suite fails if pyBYD has a public model, function, client method or car method that openbyd does not compare against. How openbyd and pyBYD compare on speed and memory is measured in [bench](bench/README.md).

## Use it from Python

```python
import asyncio
import openbyd

async def main():
    async with openbyd.Client(username="you@example.com", password="...") as client:
        await client.login()
        for vehicle in await client.get_vehicles():
            realtime = await client.get_vehicle_realtime(vehicle["vin"])
            print(vehicle["vin"], realtime["elec_percent"])

asyncio.run(main())
```

Methods and arguments are pyBYD's. Results are plain dicts shaped like pyBYD's `model_dump(mode="json")`, and errors are raised as pyBYD's exception classes (`openbyd.BydApiError` and its subclasses, with `code` and `endpoint`).

## Use it from Rust

```rust
use openbyd_client::{client::BydClient, http::HttpPool};
use openbyd_protocol::config::BydConfig;

let pool = HttpPool::new(); // one per process, shared by every client
let client = BydClient::connect(BydConfig::new("you@example.com", "..."), &pool);
client.login().await?;
for vehicle in client.get_vehicles().await? {
    let realtime = client.get_vehicle_realtime(&vehicle.vin, Default::default()).await?;
    println!("{} {:?}", vehicle.vin, realtime.elec_percent);
}
```

## Speed up pyBYD today

pyBYD encrypts every request and decrypts every reply with a table-driven cipher written in Python. A 2 KB request and reply cost it about 130 ms of CPU, during which the event loop is blocked. openbyd does the same work in under half a millisecond and releases the GIL while it runs.

```python
from openbyd import pybyd_accel

pybyd_accel.install()  # call once, before creating a BydClient
```

`install()` returns `True` when pyBYD is using the Rust cipher. It first checks that pyBYD's tables are the ones built into openbyd and that both ciphers give the same answer on a self-test. If either check fails it changes nothing and returns `False`. A client given its own table file keeps using pyBYD's code.

## Use the cipher directly

Python:

```python
import openbyd

envelope = openbyd.encode_envelope('{"hello":"world"}')
plaintext = openbyd.decode_envelope(envelope)
```

Rust:

```rust
let envelope = openbyd_crypto::encode_envelope(br#"{"hello":"world"}"#);
let plaintext = openbyd_crypto::decode_envelope(&envelope)?;
```

## Build from source

You need a Rust toolchain and Python 3.11 or newer.

```sh
python -m venv .venv
.venv/bin/pip install maturin pytest
.venv/bin/maturin develop --release
```

## Tests

```sh
cargo test -p openbyd-crypto   # Rust, against vectors recorded from pyBYD
pytest                         # Python bindings, same vectors
```

With pyBYD installed, `pytest` also compares openbyd against it on random inputs, and checks that the recorded vectors are still what pyBYD produces:

```sh
pip install "pybyd @ git+https://github.com/jkaberg/pyBYD@$(cat parity/UPSTREAM_COMMIT)"
pytest
```

To follow upstream, change `parity/UPSTREAM_COMMIT`, reinstall pyBYD, and run `python parity/gen_bangcle_vectors.py`. Any test that then fails marks behaviour that changed upstream.

## Credits

- [pyBYD](https://github.com/jkaberg/pyBYD) by Joel Kåberg and contributors, which this project ports.
- [Niek/BYD-re](https://github.com/Niek/BYD-re), the reverse-engineering work pyBYD builds on.

## Licence

MIT. See [LICENSE](LICENSE).
