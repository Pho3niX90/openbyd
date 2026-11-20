//! Builds the optimized word tables from the same vendor file the reference
//! path reads. No generated table is checked into the source tree.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

#[path = "build/compile.rs"]
mod compile;
#[allow(dead_code)] // The runtime also uses these error variants.
#[path = "src/error.rs"]
mod error;
#[allow(dead_code)] // The runtime also exposes an embedded-table cache.
#[path = "src/tables.rs"]
mod tables;

use error::Error;
use tables::Tables;

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=data/bangcle_tables.bin");
    println!("cargo:rerun-if-changed=build/compile.rs");
    println!("cargo:rerun-if-changed=src/tables.rs");
    println!("cargo:rerun-if-changed=src/error.rs");
    let tables = Tables::from_bytes(Tables::embedded_bytes()).expect("valid vendor tables");
    let path = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let mut generated = BufWriter::new(File::create(path.join("compiled_tables.rs"))?);
    for (name, cipher) in [
        ("ENCRYPT", compile::Cipher::encrypt(&tables)),
        ("DECRYPT", compile::Cipher::decrypt(&tables)),
    ] {
        let cipher = cipher.expect("each embedded XOR page must match its decoded operands");
        writeln!(
            generated,
            "pub(super) static {name}: Cipher = Cipher {{ rounds: &{:?}, last: &{:?}, shifts: {:?}, last_shifts: {:?} }};",
            cipher.rounds, cipher.last, cipher.shifts, cipher.last_shifts,
        )?;
    }
    generated.flush()
}
