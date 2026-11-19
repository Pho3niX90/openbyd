// The build script compiles this file too (`build.rs` includes it by path, to
// read the same table file): keep it free of other modules of the crate
// except `crate::Error`.

use std::sync::OnceLock;

use crate::Error;

// Table file layout:
//   magic   b"BGTB"
//   version u16 LE
//   count   u16 LE (8)
//   index   8 x (offset u32 LE, length u32 LE)
//   data    the tables, back to back
const MAGIC: &[u8; 4] = b"BGTB";
const VERSION: u16 = 1;
const TABLE_COUNT: usize = 8;
const HEADER_SIZE: usize = 8;
const INDEX_ENTRY_SIZE: usize = 8;

pub(crate) const ROUND_LEN: usize = 0x28000;
pub(crate) const XOR_LEN: usize = 0x3C000;
pub(crate) const LAST_LEN: usize = 0x1000;
pub(crate) const PERM_LEN: usize = 8;

const NAMES: [&str; TABLE_COUNT] = [
    "inv_round",
    "inv_xor",
    "inv_first",
    "round",
    "xor",
    "final",
    "perm_decrypt",
    "perm_encrypt",
];

static EMBEDDED: &[u8] = include_bytes!("../data/bangcle_tables.bin");

/// The lookup tables that drive the white-box cipher.
#[derive(Debug, Clone, Copy)]
pub struct Tables<'a> {
    pub(crate) inv_round: &'a [u8; ROUND_LEN],
    pub(crate) inv_xor: &'a [u8; XOR_LEN],
    pub(crate) inv_first: &'a [u8; LAST_LEN],
    pub(crate) round: &'a [u8; ROUND_LEN],
    pub(crate) xor: &'a [u8; XOR_LEN],
    pub(crate) last: &'a [u8; LAST_LEN],
    pub(crate) perm_decrypt: &'a [u8; PERM_LEN],
    pub(crate) perm_encrypt: &'a [u8; PERM_LEN],
}

impl Tables<'static> {
    /// The tables shipped with this crate.
    ///
    /// Pass this value itself to the cipher functions: they recognise it by
    /// address and use the round tables folded at build time, which are
    /// about ten times faster. A copy of it, or the same file parsed again
    /// with [`Tables::from_bytes`], gives the same output through the
    /// vendor's own steps.
    pub fn embedded() -> &'static Tables<'static> {
        static PARSED: OnceLock<Tables<'static>> = OnceLock::new();
        PARSED
            .get_or_init(|| Tables::from_bytes(EMBEDDED).expect("the embedded table file is valid"))
    }

    /// The raw table file shipped with this crate.
    pub fn embedded_bytes() -> &'static [u8] {
        EMBEDDED
    }
}

impl<'a> Tables<'a> {
    /// Reads a table file.
    ///
    /// For the file shipped with this crate use [`Tables::embedded`], which
    /// the cipher runs about ten times faster.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, Error> {
        let bad = |why: String| Error::BadTables(why);

        if data.len() < HEADER_SIZE + TABLE_COUNT * INDEX_ENTRY_SIZE {
            return Err(bad("file too short".into()));
        }
        if &data[..4] != MAGIC {
            return Err(bad("bad magic".into()));
        }
        let version = u16::from_le_bytes([data[4], data[5]]);
        if version != VERSION {
            return Err(bad(format!("unsupported version {version}")));
        }
        let count = usize::from(u16::from_le_bytes([data[6], data[7]]));
        if count != TABLE_COUNT {
            return Err(bad(format!("expected {TABLE_COUNT} tables, got {count}")));
        }

        let table = |i: usize| -> Result<&'a [u8], Error> {
            let entry = HEADER_SIZE + i * INDEX_ENTRY_SIZE;
            let word = |at: usize| {
                u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize
            };
            let (offset, length) = (word(entry), word(entry + 4));
            offset
                .checked_add(length)
                .and_then(|end| data.get(offset..end))
                .ok_or_else(|| bad(format!("table {} runs past the end of the file", NAMES[i])))
        };
        fn sized<'a, const N: usize>(name: &str, bytes: &'a [u8]) -> Result<&'a [u8; N], Error> {
            bytes.try_into().map_err(|_| {
                Error::BadTables(format!(
                    "table {name}: expected {N} bytes, got {}",
                    bytes.len()
                ))
            })
        }

        Ok(Self {
            inv_round: sized(NAMES[0], table(0)?)?,
            inv_xor: sized(NAMES[1], table(1)?)?,
            inv_first: sized(NAMES[2], table(2)?)?,
            round: sized(NAMES[3], table(3)?)?,
            xor: sized(NAMES[4], table(4)?)?,
            last: sized(NAMES[5], table(5)?)?,
            perm_decrypt: sized(NAMES[6], table(6)?)?,
            perm_encrypt: sized(NAMES[7], table(7)?)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tables_parse() {
        let tables = Tables::embedded();
        assert_eq!(tables.perm_encrypt.len(), PERM_LEN);
    }

    #[test]
    fn rejects_damaged_files() {
        let good = Tables::embedded_bytes();
        assert!(matches!(
            Tables::from_bytes(&good[..40]),
            Err(Error::BadTables(_))
        ));
        assert!(matches!(
            Tables::from_bytes(&good[..good.len() - 1]),
            Err(Error::BadTables(_))
        ));

        let mut wrong_magic = good.to_vec();
        wrong_magic[0] = b'X';
        assert!(matches!(
            Tables::from_bytes(&wrong_magic),
            Err(Error::BadTables(_))
        ));

        let mut wrong_version = good.to_vec();
        wrong_version[4] = 2;
        assert!(matches!(
            Tables::from_bytes(&wrong_version),
            Err(Error::BadTables(_))
        ));

        // First table's length field: a wrong size must not be accepted.
        let mut wrong_length = good.to_vec();
        wrong_length[HEADER_SIZE + 4] ^= 1;
        assert!(matches!(
            Tables::from_bytes(&wrong_length),
            Err(Error::BadTables(_))
        ));
    }
}
