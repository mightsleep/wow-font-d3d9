//! Cache records: what is written reads back, and a bundle cut short or
//! damaged reads up to its last good record (cache::open cuts it there).

use crate::record::{EMPTY, Key, PLACEMENT, WORDS, encode, read};

fn sample() -> Vec<(Key, u8, Vec<u16>)> {
    let key = |code, dx, top, w, h| Key { code, dx, top, w, h };
    vec![
        (key(0x41, 1, 3, 17, 34), WORDS, (0..17 * 34u32).map(|i| (i.wrapping_mul(2654435761u32) >> 16) as u16).collect()),
        (key(0x20, 0, 0, 0, 0), EMPTY, vec![]),
        (key(0x4e00, 0, 2, 3, 1), WORDS, vec![0x8001, 0x1234, 0xffff]),
        (key(u32::MAX, 1, 1, 0, 0), PLACEMENT, vec![]),
        (key(0x159, 2, 5, 1, 1), WORDS, vec![0x7abc]),
    ]
}

/// Records from `at` on until one does not read: (key, kind, words) each.
fn read_all(d: &[u8]) -> Vec<(Key, u8, Vec<u16>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some((k, kind, words, n, next)) = read(d, at) {
        let w = d[words..words + 2 * n].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        out.push((k, kind, w));
        at = next;
    }
    out
}

fn bundle() -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut d = Vec::new();
    let mut spans = Vec::new();
    for (k, kind, w) in sample() {
        let r = encode(k, kind, &w);
        // The record's own bytes; the rest is padding to 4.
        spans.push((d.len(), d.len() + 20 + 2 * w.len()));
        d.extend(r);
    }
    (d, spans)
}

#[test]
fn records_read_back() {
    let (d, _) = bundle();
    assert_eq!(d.len() % 4, 0);
    assert_eq!(read_all(&d), sample());
}

#[test]
fn a_cut_keeps_the_whole_records_before_it() {
    let (d, spans) = bundle();
    for len in 0..=d.len() {
        let whole = spans.iter().take_while(|s| s.1 <= len).count();
        assert_eq!(read_all(&d[..len]), sample()[..whole], "cut at {}", len);
    }
}

#[test]
fn a_damaged_byte_ends_the_bundle_at_its_record() {
    let (d, spans) = bundle();
    for (r, &(start, end)) in spans.iter().enumerate() {
        for i in start..end {
            let mut bad = d.clone();
            bad[i] ^= 0x10;
            assert_eq!(read_all(&bad), sample()[..r], "byte {} of record {}", i - start, r);
        }
    }
}
