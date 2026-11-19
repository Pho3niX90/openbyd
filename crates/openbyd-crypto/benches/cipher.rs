//! Run with `cargo bench -p openbyd-crypto --bench cipher [-- <filter>]`.
//! Sizes describe plaintext bytes. Envelopes add padding, base64 and a prefix.

use std::hint::black_box;
use std::time::{Duration, Instant};

use openbyd_crypto::{Tables, decode_envelope, decrypt_cbc, encode_envelope, encrypt_cbc};

// Benchmark the private block and base64 routines without adding a public API.
use openbyd_crypto::Error;
// Cargo enables cfg(test) for benches, but the custom harness runs no unit tests.
#[allow(unused_imports)]
#[path = "../src/base64.rs"]
mod base64;
#[allow(unused_imports, dead_code)]
#[path = "../src/block.rs"]
mod block;
#[allow(unused_imports, dead_code)]
#[path = "../src/tables.rs"]
mod tables;

const SAMPLES: usize = 15;
const SAMPLE_TIME: Duration = Duration::from_millis(40);
const IV: [u8; 16] = [0; 16];

fn measure<T>(name: &str, filter: Option<&str>, mut run: impl FnMut() -> T) {
    if filter.is_some_and(|filter| !name.contains(filter)) {
        return;
    }
    let mut iterations = 1;
    loop {
        let start = Instant::now();
        for _ in 0..iterations {
            black_box(run());
        }
        if start.elapsed() >= SAMPLE_TIME {
            break;
        }
        iterations *= 2;
    }
    let mut samples = [0.0; SAMPLES];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..iterations {
            black_box(run());
        }
        *sample = start.elapsed().as_secs_f64() * 1e9 / f64::from(iterations);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{name:24} {:10.1} ns (min {:10.1}, max {:10.1})",
        samples[SAMPLES / 2],
        samples[0],
        samples[SAMPLES - 1]
    );
}

fn main() {
    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let filter = filter.as_deref();
    let block_tables = tables::Tables::embedded();
    let tables = Tables::embedded();
    let mut seed = 0x9E37_79B9_u32;
    let plain: Vec<_> = (0..9728)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            0x20 + (seed % 95) as u8
        })
        .collect();
    let first: &[u8; 16] = plain[..16].try_into().unwrap();
    let sealed = block::encrypt_block(block_tables, first);
    measure("block/encrypt", filter, || {
        block::encrypt_block(black_box(block_tables), black_box(first))
    });
    measure("block/decrypt", filter, || {
        block::decrypt_block(black_box(block_tables), black_box(&sealed))
    });

    for size in [512, 2048, 9728] {
        let plain = &plain[..size];
        let sealed = encrypt_cbc(tables, plain, &IV).unwrap();
        let encoded = encode_envelope(plain);
        measure(&format!("cbc/encrypt/{size}"), filter, || {
            encrypt_cbc(tables, black_box(plain), &IV).unwrap()
        });
        measure(&format!("cbc/decrypt/{size}"), filter, || {
            decrypt_cbc(tables, black_box(&sealed), &IV).unwrap()
        });
        measure(&format!("envelope/encode/{size}"), filter, || {
            encode_envelope(black_box(plain))
        });
        measure(&format!("envelope/decode/{size}"), filter, || {
            decode_envelope(black_box(&encoded)).unwrap()
        });
        measure(&format!("base64/decode/{size}"), filter, || {
            base64::decode_lenient(black_box(&encoded.as_bytes()[1..])).unwrap()
        });
        measure(&format!("base64/encode/{size}"), filter, || {
            base64::encode(black_box(&sealed))
        });
    }
}
