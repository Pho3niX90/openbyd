//! The pure part of pyBYD's `_api` package: the fields every request carries
//! and the API code sets (`_api/_common.py`). The endpoint functions, which
//! post requests and interpret replies, are ported in `openbyd-client`'s
//! `api` module.

pub mod common;

use serde_json::Value;

use crate::parity::Outcome;

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    common::parity_call(name, args)
}
