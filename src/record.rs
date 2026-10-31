//! A bundle's records (cache.rs), as bytes: code u32; dx, top, w, h u16;
//! kind u8, pad; word count u16; Adler-32 over all that and the words; the
//! words; the whole padded to 4.

/// A cell in its bundle: the glyph, where the client pastes it, its size.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub code: u32,
    pub dx: u32,
    pub top: u32,
    pub w: u32,
    pub h: u32,
}

const LEN: usize = 20;

/// Record kinds: a cell's words, a glyph without an outline, and where the
/// client pastes the font (dx, dy in the key's dx and top).
pub const WORDS: u8 = 1;
pub const EMPTY: u8 = 0;
pub const PLACEMENT: u8 = 255;

/// The record at `at`: key, kind, words' offset and count, where the next
/// starts; None where it is cut short or does not check out.
pub fn read(d: &[u8], at: usize) -> Option<(Key, u8, usize, usize, usize)> {
    let h = d.get(at..at + LEN)?;
    let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]) as u32;
    let key = Key {
        code: u32::from_le_bytes(h[0..4].try_into().ok()?),
        dx: u16_at(4),
        top: u16_at(6),
        w: u16_at(8),
        h: u16_at(10),
    };
    let n = u16_at(14) as usize;
    let words = d.get(at + LEN..at + LEN + 2 * n)?;
    let mut sum = h[..16].to_vec();
    sum.extend_from_slice(words);
    let adler = u32::from_le_bytes(h[16..20].try_into().ok()?);
    (adler2::adler32_slice(&sum) == adler).then_some((
        key,
        h[12],
        at + LEN,
        n,
        (at + LEN + 2 * n + 3) & !3,
    ))
}

pub fn encode(k: Key, kind: u8, words: &[u16]) -> Vec<u8> {
    let mut r = k.code.to_le_bytes().to_vec();
    for v in [k.dx, k.top, k.w, k.h] {
        r.extend((v as u16).to_le_bytes());
    }
    r.extend([kind, 0]);
    r.extend((words.len() as u16).to_le_bytes());
    let body: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let adler = adler2::adler32_slice(&[&r[..], &body].concat());
    r.extend(adler.to_le_bytes());
    r.extend(body);
    r.resize(r.len().next_multiple_of(4), 0);
    r
}
