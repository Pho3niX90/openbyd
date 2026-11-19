//! The white-box AES block cipher.
//!
//! The vendor path keeps each state column in an 8-byte slot and evaluates
//! three nibble lookup steps per output nibble. For the embedded tables we
//! fold those steps into the word lookups at build time, then XOR four words per
//! output row. See [compiled] for the encoding and table layout.

use crate::tables::Tables;

#[path = "block/compiled.rs"]
mod compiled;
#[path = "block/vendor.rs"]
mod vendor;

pub(crate) fn encrypt_block(tables: &Tables<'_>, block: &[u8; 16]) -> [u8; 16] {
    if std::ptr::eq(tables, Tables::embedded()) {
        return compiled::ENCRYPT.apply(block);
    }
    vendor::encrypt_block(tables, block)
}

pub(crate) fn decrypt_block(tables: &Tables<'_>, block: &[u8; 16]) -> [u8; 16] {
    if std::ptr::eq(tables, Tables::embedded()) {
        return compiled::DECRYPT.apply(block);
    }
    vendor::decrypt_block(tables, block)
}

#[cfg(test)]
mod tests {
    use super::vendor as reference;
    use super::{Tables, decrypt_block, encrypt_block};
    use crate::{decrypt_cbc, encrypt_cbc};

    fn random_block(seed: &mut u64) -> [u8; 16] {
        std::array::from_fn(|_| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed as u8
        })
    }

    #[test]
    fn blocks_match_vendor_tables() {
        let tables = Tables::embedded();
        let mut seed = 0x6a09_e667_f3bc_c909;
        for i in 0..100_000 {
            let block = random_block(&mut seed);
            assert_eq!(
                encrypt_block(tables, &block),
                reference::encrypt_block(tables, &block),
                "encrypt block {i}"
            );
            assert_eq!(
                decrypt_block(tables, &block),
                reference::decrypt_block(tables, &block),
                "decrypt block {i}"
            );
        }
    }

    #[test]
    fn cbc_matches_vendor_tables_at_every_length_through_64_blocks() {
        let tables = Tables::embedded();
        // The same file parsed again is another value, so the cipher takes
        // the vendor path for it (see `encrypt_block`); the assertion below
        // makes sure this stays a comparison of two different paths.
        let uncompiled = Tables::from_bytes(Tables::embedded_bytes()).unwrap();
        assert!(!std::ptr::eq(tables, &uncompiled));
        let mut seed = 0xbb67_ae85_84ca_a73b;
        for blocks in 0..=64 {
            let iv = random_block(&mut seed);
            let input: Vec<u8> = (0..blocks).flat_map(|_| random_block(&mut seed)).collect();
            assert_eq!(
                encrypt_cbc(tables, &input, &iv).unwrap(),
                encrypt_cbc(&uncompiled, &input, &iv).unwrap(),
                "encrypt {blocks} blocks"
            );
            assert_eq!(
                decrypt_cbc(tables, &input, &iv).unwrap(),
                decrypt_cbc(&uncompiled, &input, &iv).unwrap(),
                "decrypt {blocks} blocks"
            );
        }
    }

    #[test]
    fn custom_tables_use_their_own_xor_pages() {
        let mut data = Tables::embedded_bytes().to_vec();
        // Override both XOR tables without changing table lengths or offsets.
        for entry in [1, 4] {
            let index = 8 + 8 * entry;
            let start = u32::from_le_bytes(data[index..index + 4].try_into().unwrap()) as usize;
            data[start..start + crate::tables::XOR_LEN].fill(0);
        }
        let tables = Tables::from_bytes(&data).unwrap();
        let block = [42; 16];
        let encrypted = encrypt_block(&tables, &block);
        let decrypted = decrypt_block(&tables, &block);
        assert_eq!(encrypted, reference::encrypt_block(&tables, &block));
        assert_eq!(decrypted, reference::decrypt_block(&tables, &block));
        assert_ne!(encrypted, encrypt_block(Tables::embedded(), &block));
        assert_ne!(decrypted, decrypt_block(Tables::embedded(), &block));
    }
}
