//! Embeds the pyBYD commit this build matches (`parity/UPSTREAM_COMMIT`).

use std::path::Path;

fn main() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity/UPSTREAM_COMMIT");
    println!("cargo:rerun-if-changed={}", file.display());
    let commit = std::fs::read_to_string(&file)
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=OPENBYD_UPSTREAM_COMMIT={commit}");
}
