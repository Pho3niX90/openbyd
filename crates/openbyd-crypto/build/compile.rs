//! Derives the packed round tables from the vendor file for the build script.
//!
//! In the vendor cipher an output byte of a round is made from four table
//! bytes, one nibble at a time, by a chain of three 256-entry pages: page 0
//! combines operands 0 and 1, page 1 its result and operand 2, page 2 that
//! result and operand 3. Every page is an XOR of its two input nibbles
//! under fixed maps of the nibbles (bijections, in the vendor's tables).
//! [`split_chain`] finds those maps and writes a chain as
//!
//! ```text
//! decode(chain(a0, a1, a2, a3)) = f0(a0) ^ f1(a1) ^ f2(a2) ^ f3(a3)
//! ```
//!
//! so the four `f` maps can be applied to the table bytes ahead of time and
//! a round becomes four word lookups XORed per output row.
//!
//! The invariant between rounds: every byte of the compiled state is
//! `decode` of the vendor's byte at the same position, with one `decode`
//! per round, row, column and nibble. `restore` holds the inverses; the
//! next round's tables (and the last table) are indexed through them, so
//! they read the byte the vendor tables expect.
//!
//! Nothing is assumed about the tables. [`coordinates`] only proposes a
//! `decode` for the last page of a chain; [`split_page`] then checks the
//! identity above entry by entry, all 256 of each of the three pages (the
//! low nibble of an entry, which is all the vendor's steps use), and
//! `decode` is inverted, which fails unless it is a bijection. That is all
//! the folding needs: the operand maps `f` may be anything. A table file
//! with a page that breaks the identity makes `compile` return `None`, and
//! the build fails instead of producing a different cipher.
use std::array;

use crate::tables::{LAST_LEN, ROUND_LEN, Tables, XOR_LEN};

type Nibbles = [u8; 16];
type XorPage = [u8; 256];
type WordPage = [u32; 256];
type Round = [[WordPage; 4]; 4];

#[derive(Clone, Copy)]
struct ByteMap {
    low: Nibbles,
    high: Nibbles,
}

impl ByteMap {
    fn apply(&self, byte: u8) -> u8 {
        self.low[usize::from(byte & 15)] | (self.high[usize::from(byte >> 4)] << 4)
    }
}

pub(super) struct Cipher {
    pub rounds: Vec<Round>,
    pub last: [[u8; 256]; 16],
    pub shifts: [usize; 4],
    pub last_shifts: [usize; 4],
}

impl Cipher {
    pub(super) fn encrypt(tables: &Tables<'_>) -> Option<Self> {
        Self::compile(
            0..9,
            tables.round,
            tables.xor,
            tables.last,
            tables.perm_encrypt,
            [0, 1, 2, 3],
        )
    }

    pub(super) fn decrypt(tables: &Tables<'_>) -> Option<Self> {
        Self::compile(
            (1..=9).rev(),
            tables.inv_round,
            tables.inv_xor,
            tables.inv_first,
            tables.perm_decrypt,
            [0, 3, 2, 1],
        )
    }

    fn compile(
        rounds: impl Iterator<Item = usize>,
        words: &[u8; ROUND_LEN],
        xor: &[u8; XOR_LEN],
        last: &[u8; LAST_LEN],
        perm: &[u8; 8],
        last_shifts: [usize; 4],
    ) -> Option<Self> {
        let words = words.as_chunks::<{ 16 * 1024 }>().0;
        let xor = xor.as_chunks::<{ 96 * 256 }>().0;
        let shifts = array::from_fn(|i| usize::from(perm[i * 2]) & 3);
        let mut compiled = Vec::with_capacity(9);
        let identity = array::from_fn(|i| i as u8);
        let mut restore = [[ByteMap {
            low: identity,
            high: identity,
        }; 4]; 4];
        for round in rounds {
            compiled.push(compile_round(
                words[round].as_chunks::<1024>().0,
                xor[round].as_chunks::<256>().0,
                shifts,
                &mut restore,
            )?);
        }
        let vendor_last = last.as_chunks::<256>().0;
        let last = array::from_fn(|page| {
            array::from_fn(|byte| {
                vendor_last[page][usize::from(restore[page / 4][page % 4].apply(byte as u8))]
            })
        });
        Some(Self {
            rounds: compiled,
            last,
            shifts,
            last_shifts,
        })
    }
}

fn compile_round(
    words: &[[u8; 1024]],
    xor: &[XorPage],
    shifts: [usize; 4],
    restore: &mut [[ByteMap; 4]; 4],
) -> Option<Round> {
    let mut round = [[[0; 256]; 4]; 4];
    let previous = *restore;
    for (row, row_pages) in round.iter_mut().enumerate() {
        let mut maps = [[ByteMap {
            low: [0; 16],
            high: [0; 16],
        }; 4]; 4];
        for (col, maps) in maps.iter_mut().enumerate() {
            let page = row * 24 + col * 6;
            let (low, restore_low) = split_chain([&xor[page], &xor[page + 2], &xor[page + 4]])?;
            let (high, restore_high) =
                split_chain([&xor[page + 1], &xor[page + 3], &xor[page + 5]])?;
            *maps = array::from_fn(|i| ByteMap {
                low: low[i],
                high: high[i],
            });
            restore[row][col] = ByteMap {
                low: restore_low,
                high: restore_high,
            };
        }
        for (operand, compiled) in row_pages.iter_mut().enumerate() {
            let from = (row + shifts[operand]) & 3;
            let page = words[from * 4 + operand].as_chunks::<4>().0;
            for (byte, compiled) in compiled.iter_mut().enumerate() {
                let vendor_byte = previous[from][operand].apply(byte as u8);
                let word = page[usize::from(vendor_byte)];
                *compiled =
                    u32::from_le_bytes(array::from_fn(|col| maps[col][operand].apply(word[col])));
            }
        }
    }
    Some(round)
}

fn invert(map: Nibbles) -> Option<Nibbles> {
    let mut inverse = [0; 16];
    let mut seen = 0u16;
    for (index, value) in map.into_iter().enumerate() {
        let bit = 1 << value;
        if seen & bit != 0 {
            return None;
        }
        seen |= bit;
        inverse[usize::from(value)] = index as u8;
    }
    Some(inverse)
}

fn output(page: &XorPage, left: usize, right: usize) -> u8 {
    page[left + 16 * right] & 15
}

/// Assigns XOR coordinates to the last page's 16 possible output nibbles.
fn coordinates(page: &XorPage) -> Option<Nibbles> {
    // With either input held at zero, its partner enumerates the outputs.
    let left = invert(array::from_fn(|i| output(page, i, 0)))?;
    let right = invert(array::from_fn(|i| output(page, 0, i)))?;
    // The encoded output at (0, 0) is the origin. Pick the first value
    // outside the current span (`fresh`, reached with `right_input`) and
    // translate the span by it.
    let origin = output(page, 0, 0);
    let mut values = [origin; 16];
    let mut seen = 1u16 << origin;
    let mut len = 1;
    for (fresh, &right_input) in right.iter().enumerate() {
        if seen & (1 << fresh) != 0 {
            continue;
        }
        for i in 0..len {
            let value = output(
                page,
                usize::from(left[usize::from(values[i])]),
                usize::from(right_input),
            );
            let bit = 1 << value;
            if seen & bit != 0 {
                return None;
            }
            seen |= bit;
            values[len + i] = value;
        }
        len *= 2;
        if len == 16 {
            return invert(values);
        }
    }
    None
}

/// Splits a page in the chosen coordinates and checks all 256 entries.
fn split_page(page: &XorPage, decode: Nibbles) -> Option<(Nibbles, Nibbles)> {
    let left = array::from_fn(|a| decode[usize::from(output(page, a, 0))]);
    let right = array::from_fn(|b| decode[usize::from(output(page, 0, b))] ^ left[0]);
    for (a, &lhs) in left.iter().enumerate() {
        for (b, &rhs) in right.iter().enumerate() {
            if decode[usize::from(output(page, a, b))] != lhs ^ rhs {
                return None;
            }
        }
    }
    Some((left, right))
}

fn split_chain(pages: [&XorPage; 3]) -> Option<([Nibbles; 4], Nibbles)> {
    // Each page's left operand is the preceding page's output. Decode that
    // output in the coordinates its consumer expects, so the carry maps
    // cancel and only the four independent operand maps remain.
    let decode = coordinates(pages[2])?;
    let (carry, fourth) = split_page(pages[2], decode)?;
    let (carry, third) = split_page(pages[1], carry)?;
    let (first, second) = split_page(pages[0], carry)?;
    Some(([first, second, third, fourth], invert(decode)?))
}
