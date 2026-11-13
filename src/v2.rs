//! Format v2 in the game: our textures beside each glyph cache, which
//! shaders/decoder.hlsl reads (phi binds them; docs/design.md):
//! - s1 the column map: per cache column and row of cells, the record,
//!   its bands and where its grid starts; rebuilt at every upload;
//! - s2 the atlas: atlas::encode's records, one per font file and
//!   character for every size, grown by doubling;
//! - s3 the cover: a byte per cache texel (two for outlined fonts, A8L8),
//!   NOT_OURS where a cell is not ours;
//! - s4 the texel words, 0 (the bands) where a cell is not ours.
//!
//! Cover bytes and words are memoised by glyph, size and place, and kept
//! on disk (cache.rs). Monochrome fonts keep the client's bitmaps.

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use crate::atlas::{self, Record};
use crate::cache::{self, FxMap, fx_map};
use crate::glyph::{self, Placement};
use crate::{HRESULT, Ptr, dev_orig, log, settings};

const DEV_CREATE_TEXTURE: usize = 23;
const TEX_LOCK_RECT: usize = 19;
const TEX_UNLOCK_RECT: usize = 20;
const FMT_L8: u32 = 50;
const FMT_A8L8: u32 = 51;
const FMT_A32B32G32R32F: u32 = 116;
const FMT_A16B16G16R16: u32 = 36;
const POOL_MANAGED: u32 = 1;
const ATLAS_W: u32 = 256;
const NO_GLYPH: [f32; 4] = [-1.0, 0.0, 0.0, 0.0];

/// Flags the curve decoder cannot draw yet: monochrome.
const NOT_YET: u32 = 0x2;

/// The device, an atomic and no lock: read under others.
static DEV: AtomicUsize = AtomicUsize::new(0);

pub fn device(dev: Ptr) {
    DEV.store(dev as usize, Relaxed);
}

/// Whether a font with these flags goes v2.
pub fn eligible(flags: u32) -> bool {
    settings::V2.get() != 0.0 && flags & NOT_YET == 0 && crate::draw::decoder() != 0
}

#[repr(C)]
struct LockedRect {
    pitch: i32,
    bits: *mut u8,
}

/// A managed texture of ours (not the client's: CreateTexture through the
/// real method, so no hook adopts it).
fn create(w: u32, h: u32, format: u32) -> Option<usize> {
    let dev = DEV.load(Relaxed);
    if dev == 0 {
        return None;
    }
    let f: unsafe extern "system" fn(Ptr, u32, u32, u32, u32, u32, u32, *mut Ptr, Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_CREATE_TEXTURE)) };
    let mut tex: Ptr = std::ptr::null_mut();
    let r = unsafe {
        f(
            dev as Ptr,
            w,
            h,
            1,
            0,
            format,
            POOL_MANAGED,
            &mut tex,
            std::ptr::null_mut(),
        )
    };
    if r < 0 || tex.is_null() {
        log(&format!(
            "v2: CreateTexture {}x{} format {} -> {:#x}",
            w, h, format, r
        ));
        return None;
    }
    Some(tex as usize)
}

/// Rows y0..y1 of texture `tex` (256 texels wide, as all of ours are) from
/// `bytes` (`row` bytes a row), through the texture's own Lock/UnlockRect.
fn upload(tex: usize, bytes: &[u8], row: usize, y0: u32, y1: u32) {
    if tex == 0 || y1 <= y0 {
        return;
    }
    unsafe {
        let vt = *(tex as *const *const usize);
        let lock: unsafe extern "system" fn(
            usize,
            u32,
            *mut LockedRect,
            *const [i32; 4],
            u32,
        ) -> HRESULT = std::mem::transmute(*vt.add(TEX_LOCK_RECT));
        let unlock: unsafe extern "system" fn(usize, u32) -> HRESULT =
            std::mem::transmute(*vt.add(TEX_UNLOCK_RECT));
        let rect = [0, y0 as i32, 256, y1 as i32];
        let mut l = LockedRect {
            pitch: 0,
            bits: std::ptr::null_mut(),
        };
        if lock(tex, 0, &mut l, &rect, 0) < 0 || l.bits.is_null() {
            return;
        }
        for y in y0..y1 {
            let dst = l.bits.offset((y - y0) as isize * l.pitch as isize);
            std::ptr::copy_nonoverlapping(bytes[y as usize * row..].as_ptr(), dst, row);
        }
        unlock(tex, 0);
    }
}

unsafe fn release(obj: usize) {
    if obj != 0 {
        unsafe {
            let vt = *(obj as *const *const usize);
            let f: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vt.add(2));
            f(obj);
        }
    }
}

// ---- the atlas ----------------------------------------------------------

struct Atlas {
    texels: Vec<[f32; 4]>,
    tex: usize,
    /// Rows the texture has, and how many texels of `texels` it holds.
    rows: u32,
    uploaded: usize,
}

static ATLAS: Mutex<Atlas> = Mutex::new(Atlas {
    texels: Vec::new(),
    tex: 0,
    rows: 0,
    uploaded: 0,
});

/// A glyph in the atlas: its record's first texel, the record (its pieces
/// for the cover bytes), its outline's box and the font's units an em.
struct Entry {
    slot: u32,
    record: Record,
    bbox: [f32; 4],
}

/// By (font file fingerprint, character); None for one with no outline.
static GLYPHS: Mutex<FxMap<(u64, u32, bool), Option<Arc<Entry>>>> = Mutex::new(fx_map());

/// The font's grid scale (its box onto the grid) and units an em, by
/// fingerprint: one scale a font, so cache texels to grid steps is one
/// constant a draw.
static FONTS: Mutex<FxMap<u64, (f32, f32)>> = Mutex::new(fx_map());

fn font_info(fp: u64, data: &[u8]) -> Option<(f32, f32)> {
    if let Some(&v) = FONTS.lock().unwrap().get(&fp) {
        return Some(v);
    }
    let face = ttf_parser::Face::parse(data, 0).ok()?;
    let bb = face.global_bounding_box();
    let side = ((bb.x_max - bb.x_min) as f32).max((bb.y_max - bb.y_min) as f32);
    if side.is_nan() || side <= 0.0 {
        return None;
    }
    let v = ((atlas::GRID - 1.0) / side, face.units_per_em() as f32);
    FONTS.lock().unwrap().insert(fp, v);
    Some(v)
}

/// A glyph's entry: one record for plain cells, one for outlined (whose
/// lists keep the pieces that add no area, for the distance).
fn entry(fp: u64, data: &[u8], code: u32, scale: f32, outlined: bool) -> Option<Arc<Entry>> {
    if let Some(e) = GLYPHS.lock().unwrap().get(&(fp, code, outlined)) {
        return e.clone();
    }
    // The record and the outline's box from the disk if they are there
    // (or known to be none: no outline), else worked out and kept.
    let bundle = cache::Params::glyphs(fp);
    let key = cache::Key {
        code,
        dx: outlined as u32,
        top: 0,
        w: 0,
        h: 0,
    };
    let kept = match cache::get(&bundle, key) {
        Some(Some(words)) => atlas::Record::unpack(&words, scale).map(Some),
        Some(None) => Some(None),
        None => None,
    };
    let made = kept.unwrap_or_else(|| {
        let made = glyph::outline(data, code).and_then(|g| {
            let record = atlas::encode(&g.contours, Some(scale), outlined)?;
            Some((record, g.bbox))
        });
        let words = made.as_ref().map(|(r, bbox)| r.pack(*bbox));
        // A record counts its words in 16 bits.
        if words.as_ref().is_none_or(|w| w.len() <= u16::MAX as usize) {
            cache::put(&bundle, key, words.as_deref());
        }
        made
    });
    let e = made.map(|(record, bbox)| {
        let mut a = ATLAS.lock().unwrap();
        let slot = a.texels.len() as u32;
        a.texels.extend_from_slice(&record.texels);
        Arc::new(Entry { slot, record, bbox })
    });
    GLYPHS
        .lock()
        .unwrap()
        .insert((fp, code, outlined), e.clone());
    e
}

/// Put the atlas texels not yet on the GPU there, growing the texture by
/// doubling (all of it then).
fn sync_atlas() {
    let mut a = ATLAS.lock().unwrap();
    if a.uploaded == a.texels.len() && a.tex != 0 {
        return;
    }
    let need = (a.texels.len() as u32).div_ceil(ATLAS_W).max(1);
    if need > a.rows {
        let rows = need.next_power_of_two().max(64);
        let Some(tex) = create(ATLAS_W, rows, FMT_A32B32G32R32F) else {
            return;
        };
        unsafe { release(a.tex) };
        a.tex = tex;
        a.rows = rows;
        a.uploaded = 0;
        log(&format!(
            "v2: atlas {}x{} for {} texels",
            ATLAS_W,
            rows,
            a.texels.len()
        ));
    }
    let (y0, y1) = (a.uploaded as u32 / ATLAS_W, need);
    let mut bytes = vec![0u8; (y1 * ATLAS_W * 16) as usize];
    let filled = (a.texels.len() * 16).min(bytes.len());
    bytes[..filled].copy_from_slice(unsafe {
        std::slice::from_raw_parts(a.texels.as_ptr() as *const u8, filled)
    });
    upload(a.tex, &bytes, (ATLAS_W * 16) as usize, y0, y1);
    a.uploaded = a.texels.len();
}

// ---- a cache's column map and cover texture -----------------------------

struct Cache {
    header: u32,
    cell_height: u32,
    rows: u32,
    /// Grid steps a cache texel.
    k: f32,
    /// The outline's radius in texels, 0 for a plain font, whose cover is
    /// L8 (a byte a texel); an outlined one's is A8L8 (two).
    radius: f32,
    map: usize,
    cover: usize,
    words: usize,
    map_texels: Vec<[f32; 4]>,
    cover_bytes: Vec<u8>,
    /// A word a cache texel, x y z w as the texture takes them.
    word_data: Vec<[u16; 4]>,
}

impl Cache {
    fn per(&self) -> usize {
        if self.radius > 0.0 { 2 } else { 1 }
    }
}

static CACHES: Mutex<FxMap<usize, Cache>> = Mutex::new(fx_map());

/// A cell's cover bytes and texel words by (fingerprint, character, pixel
/// size, dx, top, width, cell height, radius bits); cleared with the
/// payload's encodings (the gamma, the outline setting).
type CoverKey = (u64, u32, u32, u32, u32, u32, u32, u32);
type Baked = (Arc<[u8]>, Arc<[[u16; 4]]>);
static COVER: Mutex<FxMap<CoverKey, Baked>> = Mutex::new(fx_map());

pub fn forget_encodings() {
    COVER.lock().unwrap().clear();
}

/// The texture at this address is gone or another: our textures for it go.
pub fn forget(tex: usize) {
    if let Some(c) = CACHES.lock().unwrap().remove(&tex) {
        unsafe {
            release(c.map);
            release(c.cover);
            release(c.words);
        }
    }
}

/// The outline's radius in texels for a font's flags (phi's EDGE).
pub fn radius_of(flags: u32) -> f32 {
    let outline = if flags & 0x8 != 0 {
        2
    } else {
        (flags & 0x1 != 0) as u32
    };
    atlas::outline_radius(outline, settings::OUTLINE.get())
}

/// A blank cover: NOT_OURS everywhere (and alpha 0 for an outlined one).
fn blank(per: usize) -> Vec<u8> {
    if per == 2 {
        [atlas::NOT_OURS, 0].repeat(256 * 256)
    } else {
        vec![atlas::NOT_OURS; 256 * 256]
    }
}

/// Start a pass over glyph cache `tex` for the font with header `header`:
/// its map emptied and its cover all NOT_OURS. False if v2 cannot take it
/// (no textures, no font data).
pub fn begin(
    tex: usize,
    header: u32,
    cell_height: u32,
    pixel_size: u32,
    flags: u32,
    fp: u64,
    data: &[u8],
) -> bool {
    let Some((scale, upem)) = font_info(fp, data) else {
        return false;
    };
    let k = scale * upem / pixel_size as f32;
    let radius = radius_of(flags);
    let per = if radius > 0.0 { 2 } else { 1 };
    let rows = 256u32.div_ceil(cell_height.max(1));
    let mut caches = CACHES.lock().unwrap();
    let fresh = caches.get(&tex).is_none_or(|c| {
        c.cell_height != cell_height || c.per() != per || c.map == 0 || c.cover == 0 || c.words == 0
    });
    if fresh {
        if let Some(old) = caches.remove(&tex) {
            unsafe {
                release(old.map);
                release(old.cover);
                release(old.words);
            }
        }
        let format = if per == 2 { FMT_A8L8 } else { FMT_L8 };
        let (map, cover, words) = (
            create(256, rows, FMT_A32B32G32R32F),
            create(256, 256, format),
            create(256, 256, FMT_A16B16G16R16),
        );
        let (Some(map), Some(cover), Some(words)) = (map, cover, words) else {
            unsafe {
                release(map.unwrap_or(0));
                release(cover.unwrap_or(0));
                release(words.unwrap_or(0));
            }
            return false;
        };
        caches.insert(
            tex,
            Cache {
                header,
                cell_height,
                rows,
                k,
                radius,
                map,
                cover,
                words,
                map_texels: vec![NO_GLYPH; (256 * rows) as usize],
                cover_bytes: blank(per),
                word_data: vec![[0; 4]; 256 * 256],
            },
        );
    }
    let c = caches.get_mut(&tex).unwrap();
    c.header = header;
    c.k = k;
    c.radius = radius;
    c.map_texels.fill(NO_GLYPH);
    c.cover_bytes = blank(per);
    c.word_data.fill([0; 4]);
    true
}

/// A cell's bytes and words as a bundle's record keeps them: the bytes two
/// to a word (the last padded), then the words.
fn pack(bytes: &[u8], words: &[[u16; 4]]) -> Vec<u16> {
    let mut v: Vec<u16> = bytes
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0)]))
        .collect();
    v.extend(words.iter().flatten());
    v
}

/// pack's inverse for `n` bytes and `texels` words; None for a record of
/// another length.
fn unpack(v: &[u16], n: usize, texels: usize) -> Option<Baked> {
    let split = n.div_ceil(2);
    if v.len() != split + 4 * texels {
        return None;
    }
    let bytes: Arc<[u8]> = v[..split]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .take(n)
        .collect();
    let words: Arc<[[u16; 4]]> = v[split..].as_chunks::<4>().0.iter().copied().collect();
    Some((bytes, words))
}

/// One cell of a pass: the glyph `code` placed `at` in the cell at `start`
/// on row `row`, `w` texels wide; `bundle` keeps its bytes and words on
/// disk. False for a glyph with no outline (its cell stays the client's).
#[allow(clippy::too_many_arguments)]
pub fn cell(
    tex: usize,
    fp: u64,
    data: &[u8],
    code: u32,
    at: &Placement,
    start: u32,
    row: u32,
    w: u32,
    bundle: &cache::Params,
) -> bool {
    let Some((scale, upem)) = font_info(fp, data) else {
        return false;
    };
    // One lock at a time: the cache's numbers, then the bytes, then the
    // cache again to write.
    let Some((ch, k, radius)) = CACHES
        .lock()
        .unwrap()
        .get(&tex)
        .map(|c| (c.cell_height, c.k, c.radius))
    else {
        return false;
    };
    let outlined = radius > 0.0;
    let Some(e) = entry(fp, data, code, scale, outlined) else {
        return false;
    };
    let s = at.pixel_size as f32 / upem;
    // A cell texel X is the outline's x * s - left + dx from the cell's
    // start, a texel Y (down) top - y * s + at.top from its row: grid steps
    // are (X - ox) * k and (oy - Y) * k (bench.rs does the same).
    let (left, top) = ((e.bbox[0] * s).floor(), (e.bbox[3] * s).ceil());
    let y0 = row * ch;
    let ox = start as f32 + at.dx as f32 - left + e.record.origin[0] * s;
    let oy = y0 as f32 + at.top as f32 + top - e.record.origin[1] * s;
    let head = e.record.texels[0];
    let texel = [e.slot as f32, head[1] * 16.0 + head[0] - 1.0, ox, oy];
    // The bytes depend on the glyph's place in its cell, not the cell's in
    // the cache.
    let key = (
        fp,
        code,
        at.pixel_size,
        at.dx,
        at.top,
        w,
        ch,
        radius.to_bits(),
    );
    let per = if outlined { 2 } else { 1 };
    let texels = (w * ch) as usize;
    let disk_key = cache::Key {
        code,
        dx: at.dx,
        top: at.top,
        w,
        h: ch,
    };
    // The memo's guard is a temporary: in the same statement as or_else it
    // would live through the closure, which locks COVER again.
    let memo = COVER.lock().unwrap().get(&key).cloned();
    let baked = memo.or_else(|| {
        let v = cache::get(bundle, disk_key)??;
        let b = unpack(&v, texels * per, texels)?;
        COVER.lock().unwrap().insert(key, b.clone());
        Some(b)
    });
    let (bytes, words) = baked.unwrap_or_else(|| {
        let at = [ox - start as f32, oy - y0 as f32];
        let b: Arc<[u8]> =
            atlas::cover_cell(&e.record, w, ch, at, k, radius, 1.0 / bundle.gamma).into();
        // An outlined font's words name the pieces within its radius, half
        // a texel and 1/32 of a pixel's centre too.
        let reach = if outlined {
            radius + 0.5 + 1.0 / 32.0
        } else {
            0.0
        };
        let wd: Arc<[[u16; 4]]> = atlas::texel_words(&e.record, w, ch, at, k, reach).into();
        COVER.lock().unwrap().insert(key, (b.clone(), wd.clone()));
        let v = pack(&b, &wd);
        // A record counts its words in 16 bits.
        if v.len() <= u16::MAX as usize {
            cache::put(bundle, disk_key, Some(&v));
        }
        (b, wd)
    });
    if bytes.len() != texels * per || words.len() != texels {
        return false;
    }
    let mut caches = CACHES.lock().unwrap();
    let Some(c) = caches.get_mut(&tex) else {
        return false;
    };
    for x in start..(start + w).min(256) {
        c.map_texels[(row * 256 + x) as usize] = texel;
    }
    let cols = w.min(256 - start) as usize * per;
    for ty in 0..ch {
        let y = (y0 + ty) as usize;
        if y >= 256 {
            break;
        }
        let dst = (y * 256 + start as usize) * per;
        let src = (ty * w) as usize * per;
        c.cover_bytes[dst..dst + cols].copy_from_slice(&bytes[src..src + cols]);
        let (dst, src, cols) = (dst / per, src / per, cols / per);
        c.word_data[dst..dst + cols].copy_from_slice(&words[src..src + cols]);
    }
    true
}

/// The end of a pass: the atlas, map and cover on the GPU.
pub fn commit(tex: usize) {
    sync_atlas();
    let caches = CACHES.lock().unwrap();
    let Some(c) = caches.get(&tex) else {
        return;
    };
    let map = unsafe {
        std::slice::from_raw_parts(c.map_texels.as_ptr() as *const u8, c.map_texels.len() * 16)
    };
    upload(c.map, map, 256 * 16, 0, c.rows);
    upload(c.cover, &c.cover_bytes, 256 * c.per(), 0, 256);
    let words = unsafe {
        std::slice::from_raw_parts(c.word_data.as_ptr() as *const u8, c.word_data.len() * 8)
    };
    upload(c.words, words, 256 * 8, 0, 256);
}

/// What phi binds for a v2 cache of font `header`.
pub struct Bind {
    pub map: usize,
    pub cover: usize,
    /// 0 with the words knob off: the stage reads 0, the bands.
    pub words: usize,
    pub atlas: usize,
    pub atlas_rows: u32,
    pub map_rows: u32,
    pub k: f32,
}

pub fn bind(tex: usize, header: u32) -> Option<Bind> {
    // One lock at a time (no lock is taken under another, lib's log aside).
    let (map, cover, words, map_rows, k) = {
        let caches = CACHES.lock().unwrap();
        let c = caches.get(&tex).filter(|c| c.header == header)?;
        let words = if settings::WORDS.get() != 0.0 {
            c.words
        } else {
            0
        };
        (c.map, c.cover, words, c.rows, c.k)
    };
    let (atlas, atlas_rows) = {
        let a = ATLAS.lock().unwrap();
        (a.tex, a.rows)
    };
    (atlas != 0).then_some(Bind {
        map,
        cover,
        words,
        atlas,
        atlas_rows,
        map_rows,
        k,
    })
}

/// Whether `tex` is one of our textures (phi, after a Reset or a state
/// block).
pub fn owns(tex: usize) -> bool {
    if tex == 0 {
        return false;
    }
    let cache = CACHES
        .lock()
        .unwrap()
        .values()
        .any(|c| c.map == tex || c.cover == tex || c.words == tex);
    cache || ATLAS.lock().unwrap().tex == tex
}
