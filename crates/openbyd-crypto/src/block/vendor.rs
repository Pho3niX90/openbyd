//! The white-box AES block cipher.
//!
//! The working state holds the 4x4 block column by column, each column in
//! its own 8-byte slot (only the first four bytes of a slot are used). That
//! layout, and the table indexing below, follow the vendor library.

use crate::tables::{LAST_LEN, ROUND_LEN, Tables, XOR_LEN};

type State = [u8; 32];

fn load(block: &[u8; 16]) -> State {
    let mut state = [0u8; 32];
    for col in 0..4 {
        for row in 0..4 {
            state[col * 8 + row] = block[col + row * 4];
        }
    }
    state
}

fn store(state: &State) -> [u8; 16] {
    let mut block = [0u8; 16];
    for col in 0..4 {
        for row in 0..4 {
            block[col + row * 4] = state[col * 8 + row];
        }
    }
    block
}

/// Looks up one 4-byte word per state byte.
fn lookup(table: &[u8; ROUND_LEN], perm: &[u8; 8], state: &State, round: usize) -> [u8; 64] {
    let mut words = [0u8; 64];
    for i in 0..4 {
        let shift = usize::from(perm[i * 2]);
        for j in 0..4 {
            let row = (shift + j) & 3;
            let byte = usize::from(state[i * 8 + row]);
            let at = (byte + (i + (round * 4 + row) * 4) * 256) * 4;
            words[i * 16 + j * 4..][..4].copy_from_slice(&table[at..at + 4]);
        }
    }
    words
}

/// Combines the four words of each column through the nibble XOR tables.
fn combine(table: &[u8; XOR_LEN], words: &[u8; 64], round: usize, state: &mut State) {
    for col in 0..4 {
        for row in 0..4 {
            let at = col + row * 4;
            let first = words[at];
            let mut low = first & 0x0F;
            let mut high = first & 0xF0;
            let page = row * 0x18 + round * 0x60 + col * 6;
            for step in 0..3 {
                let next = words[at + 0x10 * (step + 1)];
                let low_pair = low | (next << 4);
                let high_pair = (high >> 4) | (next & 0xF0);
                low = table[(page + step * 2) * 0x100 + usize::from(low_pair)] & 0x0F;
                high = (table[(page + step * 2 + 1) * 0x100 + usize::from(high_pair)] & 0x0F) << 4;
            }
            state[row + col * 8] = high | low;
        }
    }
}

/// The last table round, which also undoes or applies the row shift.
fn finish(table: &[u8; LAST_LEN], shifts: [usize; 4], state: &mut State) {
    let before = *state;
    for row in 0..4 {
        for (group, shift) in shifts.iter().enumerate() {
            let from = (shift + row) & 3;
            let byte = usize::from(before[group * 8 + from]);
            state[group * 8 + row] = table[byte + from * 0x400 + group * 0x100];
        }
    }
}

pub(crate) fn encrypt_block(tables: &Tables<'_>, block: &[u8; 16]) -> [u8; 16] {
    let mut state = load(block);
    for round in 0..9 {
        let words = lookup(tables.round, tables.perm_encrypt, &state, round);
        combine(tables.xor, &words, round, &mut state);
    }
    finish(tables.last, [0, 1, 2, 3], &mut state);
    store(&state)
}

pub(crate) fn decrypt_block(tables: &Tables<'_>, block: &[u8; 16]) -> [u8; 16] {
    let mut state = load(block);
    for round in (1..=9).rev() {
        let words = lookup(tables.inv_round, tables.perm_decrypt, &state, round);
        combine(tables.inv_xor, &words, round, &mut state);
    }
    finish(tables.inv_first, [0, 3, 2, 1], &mut state);
    store(&state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypt_undoes_encrypt() {
        let tables = Tables::embedded();
        let mut block = [0u8; 16];
        for seed in 0u8..=255 {
            for (i, byte) in block.iter_mut().enumerate() {
                *byte = seed
                    .wrapping_mul(31)
                    .wrapping_add((i as u8).wrapping_mul(17));
            }
            let sealed = encrypt_block(tables, &block);
            assert_ne!(sealed, block);
            assert_eq!(decrypt_block(tables, &sealed), block);
        }
    }
}
