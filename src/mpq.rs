//! MPQ archives (Blizzard's MoPaQ, formats 1 and 2), read-only, just
//! enough to pull a font out of the 3.3.5a client's Data folder. After
//! StormLib's and Ladislav Zezula's descriptions of the format.
//!
//! Names are found by hash, never listed: a file's three Storm hashes of
//! its upper-cased, backslashed path pick a slot in the hash table (its
//! size a power of two, so the start is a mask) and confirm it, probing
//! linearly; the slot names an entry of the block table, which says where
//! the file lies, how it is cut into sectors and how they are packed. Both
//! tables are encrypted with Storm's stream cipher, keyed by the hash of
//! their names; so are encrypted files, keyed by their file name.
//!
//! Sectors here are stored or zlib (miniz_oxide), which is how the client's
//! fonts are packed; other compressions are reported, not guessed at.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Storm's crypt table: 0x500 words from a linear congruential generator.
fn crypt_table() -> &'static [u32; 0x500] {
    static T: OnceLock<[u32; 0x500]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 0x500];
        let mut seed: u32 = 0x0010_0001;
        for i in 0..0x100 {
            for j in 0..5 {
                seed = (seed * 125 + 3) % 0x2A_AAAB;
                let hi = (seed & 0xFFFF) << 16;
                seed = (seed * 125 + 3) % 0x2A_AAAB;
                t[i + j * 0x100] = hi | (seed & 0xFFFF);
            }
        }
        t
    })
}

/// Hash kinds: start slot, the two check hashes, the file key.
const HASH_OFFSET: u32 = 0;
const HASH_A: u32 = 1;
const HASH_B: u32 = 2;
const HASH_KEY: u32 = 3;

/// Upper-case ASCII without a branch: subtract 32 exactly when c is in
/// 'a'..='z' (c - 'a' < 26 as an unsigned compare); '/' becomes '\'.
fn storm_char(c: u8) -> u8 {
    let up = c - (((c.wrapping_sub(b'a') < 26) as u8) << 5);
    if up == b'/' {
        b'\\'
    } else {
        up
    }
}

fn hash(name: &str, kind: u32) -> u32 {
    let t = crypt_table();
    let (mut s1, mut s2) = (0x7FED_7FEDu32, 0xEEEE_EEEEu32);
    for &c in name.as_bytes() {
        let c = storm_char(c) as u32;
        s1 = t[(kind * 0x100 + c) as usize] ^ s1.wrapping_add(s2);
        s2 = c
            .wrapping_add(s1)
            .wrapping_add(s2)
            .wrapping_add(s2 << 5)
            .wrapping_add(3);
    }
    s1
}

/// Storm's stream cipher, in place on little-endian words.
fn decrypt(data: &mut [u8], mut key: u32) {
    let t = crypt_table();
    let mut seed = 0xEEEE_EEEEu32;
    for w in data.chunks_exact_mut(4) {
        seed = seed.wrapping_add(t[0x400 + (key & 0xFF) as usize]);
        let v = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) ^ key.wrapping_add(seed);
        key = ((!key << 21).wrapping_add(0x1111_1111)) | (key >> 11);
        seed = v.wrapping_add(seed).wrapping_add(seed << 5).wrapping_add(3);
        w.copy_from_slice(&v.to_le_bytes());
    }
}

const FLAG_IMPLODE: u32 = 0x0000_0100;
const FLAG_COMPRESS: u32 = 0x0000_0200;
const FLAG_ENCRYPTED: u32 = 0x0001_0000;
const FLAG_FIX_KEY: u32 = 0x0002_0000;
const FLAG_SINGLE_UNIT: u32 = 0x0100_0000;
const FLAG_SECTOR_CRC: u32 = 0x0400_0000;
const FLAG_EXISTS: u32 = 0x8000_0000;

#[derive(Clone, Copy)]
struct Block {
    pos: u64,
    csize: u32,
    fsize: u32,
    flags: u32,
}

pub struct Archive {
    path: PathBuf,
    /// Where the archive starts in the file (a 512-aligned header).
    base: u64,
    sector: u32,
    /// (name A, name B, block index) per slot.
    hashes: Vec<(u32, u32, u32)>,
    blocks: Vec<Block>,
}

fn words(b: &[u8]) -> impl Iterator<Item = u32> + '_ {
    b.chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

impl Archive {
    pub fn open(path: &Path) -> Result<Archive, String> {
        let mut f = File::open(path).map_err(|e| e.to_string())?;
        // The header is at offset 0 or on a 512-byte boundary after it (a
        // user data block, 'MPQ\x1B', may come first).
        let mut base = 0u64;
        let mut head = [0u8; 44];
        loop {
            f.seek(SeekFrom::Start(base)).map_err(|e| e.to_string())?;
            if f.read_exact(&mut head).is_err() {
                return Err("no MPQ header".into());
            }
            if &head[..4] == b"MPQ\x1A" {
                break;
            }
            base += 512;
            if base > 1 << 20 {
                return Err("no MPQ header in the first MB".into());
            }
        }
        let w = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let h16 = |i: usize| u16::from_le_bytes([head[i], head[i + 1]]) as u64;
        let (header_size, version, shift) = (w(4), h16(12), h16(14));
        let (mut hpos, mut bpos) = (w(16) as u64, w(20) as u64);
        let (hcount, bcount) = (w(24) as usize, w(28) as usize);
        let mut hi_block = 0u64;
        if version >= 1 && header_size >= 44 {
            hi_block = u64::from_le_bytes(head[32..40].try_into().unwrap());
            hpos |= h16(40) << 32;
            bpos |= h16(42) << 32;
        }
        if !hcount.is_power_of_two() || hcount > 1 << 22 || bcount > 1 << 22 {
            return Err(format!("odd table sizes {} and {}", hcount, bcount));
        }
        let mut table = |pos: u64, n: usize, key: &str| -> Result<Vec<u8>, String> {
            let mut b = vec![0u8; n * 16];
            f.seek(SeekFrom::Start(base + pos))
                .map_err(|e| e.to_string())?;
            f.read_exact(&mut b).map_err(|e| e.to_string())?;
            decrypt(&mut b, hash(key, HASH_KEY));
            Ok(b)
        };
        let ht = table(hpos, hcount, "(hash table)")?;
        let bt = table(bpos, bcount, "(block table)")?;
        let hashes = ht.chunks_exact(16).map(|e| {
            let v: Vec<u32> = words(e).collect();
            (v[0], v[1], v[3])
        });
        let mut blocks: Vec<Block> = bt
            .chunks_exact(16)
            .map(|e| {
                let v: Vec<u32> = words(e).collect();
                Block {
                    pos: v[0] as u64,
                    csize: v[1],
                    fsize: v[2],
                    flags: v[3],
                }
            })
            .collect();
        // Format 2: the high 16 bits of each block's position.
        if hi_block != 0 {
            let mut b = vec![0u8; bcount * 2];
            f.seek(SeekFrom::Start(base + hi_block))
                .map_err(|e| e.to_string())?;
            f.read_exact(&mut b).map_err(|e| e.to_string())?;
            for (blk, hi) in blocks.iter_mut().zip(b.chunks_exact(2)) {
                blk.pos |= (u16::from_le_bytes([hi[0], hi[1]]) as u64) << 32;
            }
        }
        Ok(Archive {
            path: path.to_path_buf(),
            base,
            sector: 512 << shift,
            hashes: hashes.collect(),
            blocks,
        })
    }

    fn find(&self, name: &str) -> Option<Block> {
        let mask = self.hashes.len() - 1;
        let (a, b) = (hash(name, HASH_A), hash(name, HASH_B));
        let start = hash(name, HASH_OFFSET) as usize & mask;
        for i in 0..=mask {
            let (ha, hb, block) = self.hashes[(start + i) & mask];
            match block {
                // Empty: the end of the probe. (Deleted, 0xFFFFFFFE, goes on.)
                0xFFFF_FFFF => return None,
                _ if ha == a && hb == b => {
                    let blk = *self.blocks.get(block as usize)?;
                    return (blk.flags & FLAG_EXISTS != 0).then_some(blk);
                }
                _ => {}
            }
        }
        None
    }

    /// The file `name` (a path inside the archive), or None if it is not
    /// here; Err if it is but cannot be read.
    pub fn read(&self, name: &str) -> Option<Result<Vec<u8>, String>> {
        let blk = self.find(name)?;
        Some(self.read_block(name, blk))
    }

    fn read_block(&self, name: &str, blk: Block) -> Result<Vec<u8>, String> {
        let mut f = File::open(&self.path).map_err(|e| e.to_string())?;
        let mut raw = vec![0u8; blk.csize as usize];
        f.seek(SeekFrom::Start(self.base + blk.pos))
            .map_err(|e| e.to_string())?;
        f.read_exact(&mut raw).map_err(|e| e.to_string())?;
        let key = (blk.flags & FLAG_ENCRYPTED != 0).then(|| {
            let file = name.rsplit(['\\', '/']).next().unwrap_or(name);
            let k = hash(file, HASH_KEY);
            if blk.flags & FLAG_FIX_KEY != 0 {
                k.wrapping_add(blk.pos as u32) ^ blk.fsize
            } else {
                k
            }
        });
        let packed = blk.flags & (FLAG_COMPRESS | FLAG_IMPLODE) != 0;
        if blk.flags & FLAG_SINGLE_UNIT != 0 {
            if let Some(k) = key {
                decrypt(&mut raw, k);
            }
            return if packed && blk.csize < blk.fsize {
                unpack(&raw, blk.fsize as usize, blk.flags)
            } else {
                Ok(raw)
            };
        }
        let sector = self.sector as usize;
        let n = (blk.fsize as usize).div_ceil(sector);
        if !packed {
            if let Some(k) = key {
                for (i, s) in raw.chunks_mut(sector).enumerate() {
                    decrypt(s, k.wrapping_add(i as u32));
                }
            }
            raw.truncate(blk.fsize as usize);
            return Ok(raw);
        }
        // The sector offset table: n + 1 offsets (one more with CRCs),
        // encrypted with key - 1.
        let entries = n + 1 + (blk.flags & FLAG_SECTOR_CRC != 0) as usize;
        let mut table = raw.get(..entries * 4).ok_or("short sector table")?.to_vec();
        if let Some(k) = key {
            decrypt(&mut table, k.wrapping_sub(1));
        }
        let offs: Vec<usize> = words(&table).map(|v| v as usize).collect();
        let mut out = Vec::with_capacity(blk.fsize as usize);
        for i in 0..n {
            let want = sector.min(blk.fsize as usize - i * sector);
            let mut s = raw
                .get(offs[i]..offs[i + 1])
                .ok_or("sector out of range")?
                .to_vec();
            if let Some(k) = key {
                decrypt(&mut s, k.wrapping_add(i as u32));
            }
            if s.len() < want {
                out.extend(unpack(&s, want, blk.flags)?);
            } else {
                out.extend_from_slice(&s[..want]);
            }
        }
        Ok(out)
    }
}

/// One packed sector (or single unit) into `want` bytes.
fn unpack(s: &[u8], want: usize, flags: u32) -> Result<Vec<u8>, String> {
    if flags & FLAG_IMPLODE != 0 {
        return Err("PKWARE implode is not supported".into());
    }
    // A compression mask byte, then the data.
    match s.first() {
        Some(0x02) => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&s[1..], want)
            .ok()
            .filter(|o| o.len() == want)
            .ok_or_else(|| "bad zlib sector".to_string()),
        Some(m) => Err(format!("compression {:#04x} is not supported", m)),
        None => Err("empty sector".into()),
    }
}

/// The client's archives in the order it loads them, later ones winning:
/// base, then each patch level, the locale's after the general one of the
/// same level. Patch levels: patch = 1, patch-2 .. patch-9 = 2..9, then
/// letters (custom patches) in alphabet order.
fn priority(file: &str, locale: &str) -> Option<u32> {
    let f = file.to_ascii_lowercase();
    let stem = f.strip_suffix(".mpq")?;
    let loc = locale.to_ascii_lowercase();
    let base = [
        "common".to_string(),
        "common-2".to_string(),
        "expansion".to_string(),
        "lichking".to_string(),
        format!("locale-{}", loc),
        format!("speech-{}", loc),
        format!("expansion-locale-{}", loc),
        format!("expansion-speech-{}", loc),
        format!("lichking-locale-{}", loc),
        format!("lichking-speech-{}", loc),
    ];
    if let Some(i) = base.iter().position(|b| b == stem) {
        return Some(i as u32);
    }
    let rest = stem.strip_prefix("patch")?;
    let (localized, rest) = match rest.strip_prefix(&format!("-{}", loc)) {
        Some(r) => (1, r),
        None => (0, rest),
    };
    let level = match rest {
        "" => 1,
        _ => {
            let c = rest.strip_prefix('-')?;
            let mut ch = c.chars();
            let (x, None) = (ch.next()?, ch.next()) else {
                return None;
            };
            match x {
                '2'..='9' => x as u32 - '0' as u32,
                'a'..='z' => 10 + x as u32 - 'a' as u32,
                _ => return None,
            }
        }
    };
    Some(100 + level * 2 + localized)
}

/// The client's archives, highest priority first.
pub struct Chain {
    pub archives: Vec<(u32, Archive)>,
}

impl Chain {
    /// Every archive the client would load from `data` (its Data folder)
    /// and `data`/`locale`; the ones that fail to open are skipped and
    /// named in the second list.
    pub fn open(data: &Path, locale: &str) -> (Chain, Vec<String>) {
        let mut archives = Vec::new();
        let mut failed = Vec::new();
        for dir in [data.to_path_buf(), data.join(locale)] {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(p) = priority(&name, locale) else {
                    continue;
                };
                match Archive::open(&e.path()) {
                    Ok(a) => archives.push((p, a)),
                    Err(err) => failed.push(format!("{}: {}", name, err)),
                }
            }
        }
        archives.sort_by(|a, b| b.0.cmp(&a.0));
        (Chain { archives }, failed)
    }

    /// The file from the highest priority archive that has it.
    pub fn read(&self, name: &str) -> Option<Result<Vec<u8>, String>> {
        self.archives.iter().find_map(|(_, a)| a.read(name))
    }
}
