//! Checks the cipher against vectors produced by pyBYD.
//!
//! Regenerate them with `parity/gen_bangcle_vectors.py`.

use openbyd_crypto::{Error, Tables, decode_envelope, decrypt_cbc, encode_envelope, encrypt_cbc};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("vectors/bangcle.json")).expect("vector file is JSON")
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex digit"))
        .collect()
}

fn cases<'a>(document: &'a Value, name: &str) -> &'a Vec<Value> {
    document[name]
        .as_array()
        .unwrap_or_else(|| panic!("{name} is a list"))
}

fn text<'a>(case: &'a Value, field: &str) -> &'a str {
    case[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} is a string"))
}

fn kind(error: &Error) -> &'static str {
    match error {
        Error::EmptyInput => "empty_input",
        Error::MissingPrefix => "missing_prefix",
        Error::InvalidBase64(_) => "invalid_base64",
        Error::EmptyCiphertext => "empty_ciphertext",
        Error::BadDataLength(_) => "bad_data_length",
        Error::BadIvLength(_) => "bad_iv_length",
        Error::BadTables(_) => "bad_tables",
    }
}

#[test]
fn envelopes_match_pybyd() {
    let document = vectors();
    let list = cases(&document, "envelopes");
    assert!(list.len() > 70);
    for case in list {
        let plaintext = unhex(text(case, "plaintext"));
        let envelope = text(case, "envelope");
        assert_eq!(
            encode_envelope(&plaintext),
            envelope,
            "encode {} bytes",
            plaintext.len()
        );
        assert_eq!(
            decode_envelope(envelope).unwrap(),
            plaintext,
            "decode {} bytes",
            plaintext.len()
        );
    }
}

#[test]
fn cbc_matches_pybyd() {
    let document = vectors();
    let tables = Tables::embedded();
    for case in cases(&document, "cbc") {
        let data = unhex(text(case, "data"));
        let iv = unhex(text(case, "iv"));
        assert_eq!(
            encrypt_cbc(tables, &data, &iv).unwrap(),
            unhex(text(case, "encrypted"))
        );
        assert_eq!(
            decrypt_cbc(tables, &data, &iv).unwrap(),
            unhex(text(case, "decrypted"))
        );
    }
}

#[test]
fn envelope_clean_up_matches_pybyd() {
    let document = vectors();
    let list = cases(&document, "decode_cases");
    assert!(list.len() > 20);
    for case in list {
        let input = text(case, "input");
        let got = decode_envelope(input);
        match case.get("error").and_then(Value::as_str) {
            Some(expected) => {
                let error = got.expect_err(&format!("{input:?} should be rejected"));
                assert_eq!(kind(&error), expected, "input {input:?}");
            }
            None => assert_eq!(
                got.unwrap_or_else(|error| panic!("{input:?} was rejected: {error}")),
                unhex(text(case, "plaintext")),
                "input {input:?}"
            ),
        }
    }
}
