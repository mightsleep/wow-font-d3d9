//! v2's cover bytes and texel words on disk (Cache\wow-font-d3d9\ next to
//! Wow.exe), so a cell is worked out once, not once a session; and each
//! font's records, so neither is a glyph (Params::glyphs).
//!
//! One file a font file, size, outline and settings, and build of the
//! encoder (WOW_FONT_ENCODER, a hash of its sources: build.rs); a record a
//! cell, keyed by the glyph and where the client pastes it, plus one for
//! the placement. A font file is named by its table directory, so a 26 MB
//! font is not read to identify it.
//!
//! Records are appended by a thread of their own, each with an Adler-32: a
//! tail a crash cut short is cut off at the next open.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use crate::fontfile::FontData;
use crate::log;
pub use crate::record::Key;
use crate::record::{self, EMPTY, PLACEMENT, WORDS};

/// Rustc's FxHash: per word, rotate, xor, multiply by an odd constant. A
/// product's low bits depend only on the low bits of what was multiplied,
/// and a hash map's buckets are the low bits, so finish folds the high half
/// down.
#[derive(Default, Clone, Copy)]
pub struct Fx(u64);

impl Fx {
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..c.len()].copy_from_slice(c);
            self.add(u64::from_le_bytes(b));
        }
    }
    fn write_u8(&mut self, v: u8) {
        self.add(v as u64);
    }
    fn write_u16(&mut self, v: u16) {
        self.add(v as u64);
    }
    fn write_u32(&mut self, v: u32) {
        self.add(v as u64);
    }
    fn write_u64(&mut self, v: u64) {
        self.add(v);
    }
    fn write_usize(&mut self, v: usize) {
        self.add(v as u64);
    }
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

pub type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

pub const fn fx_map<K, V>() -> FxMap<K, V> {
    HashMap::with_hasher(BuildHasherDefault::new())
}

/// A font file's identity: its table directory (tag, checksum, offset and
/// length of each table) and its length; a collection's first font. None
/// for what is no sfnt.
pub fn fingerprint(data: &[u8]) -> Option<u64> {
    let at = match data.get(0..4)? {
        b"ttcf" => u32::from_be_bytes(data.get(12..16)?.try_into().ok()?) as usize,
        _ => 0,
    };
    let tables = u16::from_be_bytes(data.get(at + 4..at + 6)?.try_into().ok()?) as usize;
    let mut h = Fx::default();
    h.write(data.get(at..at + 12 + 16 * tables)?);
    h.write_u64(data.len() as u64);
    Some(h.finish())
}

/// What names a bundle. The cells bake in the gamma and the outline's
/// radius in texels; the outline only moves the glyph and widens the
/// cell, which the keys hold. The placement is one per bundle, though, and
/// it does depend on the outline (1,1 for an outlined font, 0,0 for the
/// same font plain), hence `flags`.
pub struct Params {
    /// fingerprint() of the font file.
    pub font: u64,
    pub pixel_size: u32,
    /// The font's flags that change where the client pastes a glyph.
    pub flags: u32,
    pub gamma: f32,
    pub radius: f32,
}

/// Outline (0x1), monochrome (0x2), thick outline (0x8); 0x4 (a font made
/// for SetTextHeight) pastes like the plain one.
pub const PASTE_FLAGS: u32 = 0xb;

impl Params {
    /// The bundle of a font's records (atlas::Record::pack), one a glyph
    /// whatever the size: keyed by the character, `dx` 1 for the record
    /// of an outlined font.
    pub fn glyphs(font: u64) -> Params {
        Params {
            font,
            pixel_size: 0,
            flags: 0,
            gamma: 0.0,
            radius: 0.0,
        }
    }

    fn id(&self) -> u64 {
        let mut h = Fx::default();
        h.write_u64(self.font);
        h.write_u32(self.pixel_size);
        h.write_u32(self.flags & PASTE_FLAGS);
        h.write_u32(self.gamma.to_bits());
        h.write_u32(self.radius.to_bits());
        h.write(env!("WOW_FONT_ENCODER").as_bytes());
        h.finish()
    }
}

const MAGIC: &[u8; 4] = b"WFDC";
/// Magic and the bundle's id.
const HEADER: usize = 12;

struct Bundle {
    data: Option<FontData>,
    /// Key -> words' offset and count (None: no outline).
    index: FxMap<Key, Option<(usize, usize)>>,
    placement: Option<(u32, u32)>,
    path: PathBuf,
}

static BUNDLES: Mutex<FxMap<u64, Bundle>> = Mutex::new(fx_map());

fn dir() -> PathBuf {
    PathBuf::from(crate::beside_exe("Cache\\wow-font-d3d9"))
}

/// Map bundle `id`'s file and index the records that check out; a tail
/// that does not is cut off, so appends follow the last good record.
fn open(id: u64) -> Bundle {
    let path = dir().join(format!("{:016x}.cells", id));
    let mut b = Bundle {
        data: None,
        index: fx_map(),
        placement: None,
        path,
    };
    let Some(d) = FontData::map(&b.path) else {
        return b;
    };
    if d.get(0..4) != Some(&MAGIC[..]) || d.get(4..12) != Some(&id.to_le_bytes()[..]) {
        drop(d);
        let _ = std::fs::remove_file(&b.path);
        log(&format!("cache: {:016x} is no bundle of ours, removed", id));
        return b;
    }
    let mut at = HEADER;
    while let Some((key, kind, words, n, next)) = record::read(&d, at) {
        match kind {
            PLACEMENT => b.placement = Some((key.dx, key.top)),
            WORDS => drop(b.index.insert(key, Some((words, n)))),
            _ => drop(b.index.insert(key, None)),
        }
        at = next;
    }
    if at < d.len() {
        // A file can be cut only when nothing maps it.
        drop(d);
        let cut = std::fs::OpenOptions::new()
            .write(true)
            .open(&b.path)
            .and_then(|f| f.set_len(at as u64));
        log(&format!(
            "cache: {:016x}: cut after {} cells ({:?})",
            id,
            b.index.len(),
            cut.err()
        ));
        b.data = FontData::map(&b.path);
    } else {
        b.data = Some(d);
    }
    b
}

fn with_bundle<T>(p: &Params, f: impl FnOnce(&mut Bundle) -> T) -> T {
    let id = p.id();
    let mut all = BUNDLES.lock().unwrap();
    let b = all.entry(id).or_insert_with(|| {
        let b = open(id);
        log(&format!(
            "cache: {:016x} ({} px): {} cells, placement {:?}",
            id,
            p.pixel_size,
            b.index.len(),
            b.placement
        ));
        b
    });
    f(b)
}

/// A cached cell: Some(None) for a glyph without an outline.
pub fn get(p: &Params, k: Key) -> Option<Option<Arc<[u16]>>> {
    with_bundle(p, |b| {
        let Some((at, n)) = *b.index.get(&k)? else {
            return Some(None);
        };
        let words = b.data.as_ref()?.get(at..at + 2 * n)?;
        Some(Some(
            words
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_le_bytes(c))
                .collect(),
        ))
    })
}

pub fn put(p: &Params, k: Key, words: Option<&[u16]>) {
    match words {
        Some(w) => append(p, record::encode(k, WORDS, w)),
        None => append(p, record::encode(k, EMPTY, &[])),
    }
}

pub fn placement(p: &Params) -> Option<(u32, u32)> {
    with_bundle(p, |b| b.placement)
}

pub fn put_placement(p: &Params, (dx, dy): (u32, u32)) {
    with_bundle(p, |b| b.placement = Some((dx, dy)));
    append(
        p,
        record::encode(
            Key {
                code: u32::MAX,
                dx,
                top: dy,
                w: 0,
                h: 0,
            },
            PLACEMENT,
            &[],
        ),
    );
}

/// To the bundle's file, by the writer thread.
fn append(p: &Params, record: Vec<u8>) {
    static TX: OnceLock<Mutex<Sender<(u64, PathBuf, Vec<u8>)>>> = OnceLock::new();
    let id = p.id();
    let path = with_bundle(p, |b| b.path.clone());
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<(u64, PathBuf, Vec<u8>)>();
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all(dir());
            for (id, path, rec) in rx {
                let r = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .and_then(|mut f| {
                        if f.metadata()?.len() == 0 {
                            f.write_all(MAGIC)?;
                            f.write_all(&id.to_le_bytes())?;
                        }
                        f.write_all(&rec)
                    });
                if let Err(e) = r {
                    log(&format!("cache: {} not written: {}", path.display(), e));
                }
            }
        });
        Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send((id, path, record));
}
