use crate::block::{decrypt_block, encrypt_block};
use crate::{Error, Tables};

/// The cipher's block size in bytes.
pub const BLOCK_SIZE: usize = 16;

fn check(data: &[u8], iv: &[u8]) -> Result<[u8; BLOCK_SIZE], Error> {
    if !data.len().is_multiple_of(BLOCK_SIZE) {
        return Err(Error::BadDataLength(data.len()));
    }
    iv.try_into().map_err(|_| Error::BadIvLength(iv.len()))
}

/// Encrypts whole blocks in CBC mode. No padding is added.
pub fn encrypt_cbc(tables: &Tables<'_>, data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Error> {
    let mut previous = check(data, iv)?;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.as_chunks::<BLOCK_SIZE>().0 {
        let mut block = previous;
        for (byte, plain) in block.iter_mut().zip(chunk) {
            *byte ^= plain;
        }
        previous = encrypt_block(tables, &block);
        out.extend_from_slice(&previous);
    }
    Ok(out)
}

/// Decrypts whole blocks in CBC mode. No padding is removed.
pub fn decrypt_cbc(tables: &Tables<'_>, data: &[u8], iv: &[u8]) -> Result<Vec<u8>, Error> {
    let mut previous = check(data, iv)?;
    let mut out = Vec::with_capacity(data.len());
    for &sealed in data.as_chunks::<BLOCK_SIZE>().0 {
        let mut block = decrypt_block(tables, &sealed);
        for (byte, mask) in block.iter_mut().zip(previous) {
            *byte ^= mask;
        }
        out.extend_from_slice(&block);
        previous = sealed;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_lengths() {
        let tables = Tables::embedded();
        assert_eq!(
            encrypt_cbc(tables, &[0; 15], &[0; 16]),
            Err(Error::BadDataLength(15))
        );
        assert_eq!(
            decrypt_cbc(tables, &[0; 17], &[0; 16]),
            Err(Error::BadDataLength(17))
        );
        assert_eq!(
            encrypt_cbc(tables, &[0; 16], &[0; 15]),
            Err(Error::BadIvLength(15))
        );
        assert_eq!(
            decrypt_cbc(tables, &[0; 16], &[]),
            Err(Error::BadIvLength(0))
        );
    }

    #[test]
    fn empty_data_is_allowed() {
        let tables = Tables::embedded();
        assert_eq!(encrypt_cbc(tables, &[], &[0; 16]), Ok(vec![]));
        assert_eq!(decrypt_cbc(tables, &[], &[0; 16]), Ok(vec![]));
    }

    #[test]
    fn chaining_makes_equal_blocks_differ() {
        let tables = Tables::embedded();
        let sealed = encrypt_cbc(tables, &[7; 32], &[0; 16]).unwrap();
        assert_ne!(sealed[..16], sealed[16..]);
        assert_eq!(decrypt_cbc(tables, &sealed, &[0; 16]).unwrap(), [7; 32]);
    }
}
