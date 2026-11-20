//! Applies the precomputed round word tables for the embedded cipher.
//!
//! The build script expresses each three-page vendor nibble chain as XOR
//! of four independently decoded nibbles. It applies the operand maps to
//! the bytes of each round word. Each round then has word pages indexed
//! by [output row][operand][input byte]. XORing four words produces the
//! row's four decoded bytes. The next round (or last-byte lookup) absorbs
//! the inverse decoding into its input index. The two directions together
//! add 296 KiB of read-only data.

use std::array;

type WordPage = [u32; 256];
type Round = [[WordPage; 4]; 4];

pub(super) struct Cipher {
    rounds: &'static [Round; 9],
    last: &'static [[u8; 256]; 16],
    shifts: [usize; 4],
    last_shifts: [usize; 4],
}

include!(concat!(env!("OUT_DIR"), "/compiled_tables.rs"));

impl Cipher {
    pub(super) fn apply(&self, block: &[u8; 16]) -> [u8; 16] {
        let mut state: [[u8; 4]; 4] = array::from_fn(|row| block.as_chunks::<4>().0[row]);
        let [a, b, c, d] = self.shifts;
        for round in self.rounds {
            state = array::from_fn(|row| {
                let pages = &round[row];
                let word = pages[0][usize::from(state[(row + a) & 3][0])]
                    ^ pages[1][usize::from(state[(row + b) & 3][1])]
                    ^ pages[2][usize::from(state[(row + c) & 3][2])]
                    ^ pages[3][usize::from(state[(row + d) & 3][3])];
                word.to_le_bytes()
            });
        }
        array::from_fn(|i| {
            let col = i % 4;
            let from = (i / 4 + self.last_shifts[col]) & 3;
            self.last[from * 4 + col][usize::from(state[from][col])]
        })
    }
}
