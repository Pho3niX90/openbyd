use std::fmt;

/// What went wrong while encoding or decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The envelope was empty once whitespace was removed.
    EmptyInput,
    /// The envelope did not start with the letter `F`.
    MissingPrefix,
    /// The text after the `F` prefix was not base64.
    InvalidBase64(&'static str),
    /// The envelope decoded to no bytes at all.
    EmptyCiphertext,
    /// The data was not a whole number of 16-byte blocks.
    BadDataLength(usize),
    /// The IV was not 16 bytes.
    BadIvLength(usize),
    /// A table file could not be read.
    BadTables(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInput => f.write_str("Bangcle input is empty"),
            Self::MissingPrefix => f.write_str("Bangcle envelope must start with \"F\""),
            Self::InvalidBase64(why) => write!(f, "Invalid base64 in Bangcle envelope: {why}"),
            Self::EmptyCiphertext => f.write_str("Bangcle ciphertext is empty"),
            Self::BadDataLength(n) => write!(f, "Data length {n} is not a multiple of 16"),
            Self::BadIvLength(n) => write!(f, "IV must be 16 bytes, got {n}"),
            Self::BadTables(why) => write!(f, "Bad Bangcle table file: {why}"),
        }
    }
}

impl std::error::Error for Error {}
