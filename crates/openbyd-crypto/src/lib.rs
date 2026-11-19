//! Cipher and envelope primitives for the BYD overseas app API.
//!
//! Every request to the API, and every reply, is wrapped in an outer
//! "Bangcle" envelope: the letter `F` followed by base64 of a white-box AES
//! ciphertext in CBC mode with a zero IV. The cipher has no key schedule; it
//! is driven by lookup tables taken from the vendor app.
//!
//! This crate is a port of the cipher in [pyBYD](https://github.com/jkaberg/pyBYD)
//! and is tested against vectors produced by it.
//!
//! ```
//! let envelope = openbyd_crypto::encode_envelope(br#"{"hello":"world"}"#);
//! assert!(envelope.starts_with('F'));
//! let plain = openbyd_crypto::decode_envelope(&envelope).unwrap();
//! assert_eq!(plain, br#"{"hello":"world"}"#);
//! ```

mod base64;
mod block;
mod cbc;
mod envelope;
mod error;
mod tables;

pub use cbc::{BLOCK_SIZE, decrypt_cbc, encrypt_cbc};
pub use envelope::{decode_envelope, decode_envelope_with, encode_envelope, encode_envelope_with};
pub use error::Error;
pub use tables::Tables;
