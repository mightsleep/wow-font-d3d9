//! Read the client's font state: which glyph sits in which cell. The proxy
//! is a 32-bit DLL in the client's own process; reads go through
//! ReadProcessMemory on that process, so a bad address is an error, not a
//! crash.
//!
//! Layouts follow whoa (src/gx/font/CGxFont.hpp) and squall (Storm), 32-bit,
//! corrected where a dump of the client disagreed (see the header constants):
//!   TEXTURECACHE (24): m_texture, m_theFace, dirty+pad, TSFixedArray rows
//!     (alloc, count, data; no vtable in this client)
//!   TEXTURECACHEROW (16): widestFreeSlot, TSList (linkoffset, terminator)
//!   CHARCODEDESC: TSHashObject (m_hashval = character code, two links,
//!     key), textureRowLink, fontGlyphLink, textureNumber, rowNumber,
//!     glyphStartPixel, glyphEndPixel, GLYPHBITMAPDATA
//! Headers are not searched for: they come from the client's list of fonts
//! (g_fonts, below).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use crate::log;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn ReadProcessMemory(
        process: *mut c_void,
        addr: *const c_void,
        buf: *mut c_void,
        len: usize,
        read: *mut usize,
    ) -> i32;
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
}

/// Fill `buf` from `addr`; false (and `buf` unspecified) where any of it
/// is not readable. Each call is a transition into Wine's (or Windows')
/// kernel side, a microsecond or so: read structures whole, not by field.
pub fn read_into(addr: u32, buf: &mut [u8]) -> bool {
    let mut got = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as usize as *const c_void,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            &mut got,
        )
    };
    ok != 0 && got == buf.len()
}

pub fn read(addr: u32, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    read_into(addr, &mut buf).then_some(buf)
}

pub fn u32_at(addr: u32) -> Option<u32> {
    let mut b = [0u8; 4];
    read_into(addr, &mut b).then(|| u32::from_le_bytes(b))
}

/// Word `i` (little-endian u32 at byte 4 i) of `b`.
fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap())
}

const NAME_LEN: u32 = 260;
pub const CACHES: u32 = 8;
pub const CACHE_SIZE: u32 = 24;

pub struct Font {
    /// Address of the header (m_cellHeight); offsets below are from here.
    pub base: u32,
    pub name: String,
    pub cell_height: u32,
    pub flags: u32,
    pub pixel_size: u32,
    /// m_requestedFontHeight (UI units) and m_pixelsPerUnit: their product
    /// should be the height asked for in screen pixels, which pixel_size
    /// is capped from.
    pub requested: f32,
    pub pixels_per_unit: f32,
    pub caches: Vec<Cache>,
}

pub struct Cache {
    pub index: u32,
    pub texture: u32,
    pub cells: Vec<Cell>,
}

pub struct Cell {
    pub code: u32,
    pub row: u32,
    pub start: u32,
    pub end: u32,
    pub glyph_width: u32,
    pub glyph_height: u32,
    /// GLYPHBITMAPDATA: m_data (8-bit FreeType coverage), m_glyphPitch,
    /// m_yStart (rows the paste skips at the top of the cell).
    pub data: u32,
    pub pitch: u32,
    pub y_start: u32,
}

fn name_at(addr: u32) -> Option<String> {
    let b = read(addr, NAME_LEN as usize)?;
    let s = b.split(|&c| c == 0).next()?;
    (!s.is_empty() && s.iter().all(|c| c.is_ascii_graphic() || *c == b' '))
        .then(|| String::from_utf8_lossy(s).into_owned())
}

// Header, from m_cellHeight on (offsets read off a dump of the 3.3.5a
// client; TSFixedArray has no vtable there, so a TEXTURECACHE is 24 bytes):
//   +0 m_cellHeight, +4 m_baseline, +8 m_flags, +12 m_requestedFontHeight,
//   +16 m_pixelsPerUnit, +20 TEXTURECACHE[8] (m_texture, m_theFace, dirty,
//   alloc, count, data), +212 m_pixelSize.
pub const CACHES_AT: u32 = 20;
const PIXEL_SIZE: u32 = CACHES_AT + CACHES * CACHE_SIZE;

/// The header from m_cellHeight through m_pixelSize, in one read.
const HEADER_LEN: usize = PIXEL_SIZE as usize + 4;

fn header(h: u32) -> Option<[u8; HEADER_LEN]> {
    let mut b = [0u8; HEADER_LEN];
    read_into(h, &mut b).then_some(b)
}

/// Row array (count, data) of texture cache `i` of header `b`: Some(None)
/// when the cache is not initialised, None when it is but does not check
/// out.
fn cache_rows(b: &[u8], i: u32, cell_height: u32) -> Option<Option<(u32, u32)>> {
    let c = ((CACHES_AT + i * CACHE_SIZE) / 4) as usize;
    if word(b, c) == 0 {
        return Some(None);
    }
    let (alloc, count, data) = (word(b, c + 3), word(b, c + 4), word(b, c + 5));
    let ok = count == 256 / cell_height && alloc >= count && alloc <= 256 && data != 0;
    ok.then_some(Some((count, data)))
}

/// Whether header `b` is a CGxFont's with at least one live texture cache:
/// m_cellHeight 6..=64, small flags, a sane requested height, and every
/// initialised cache with exactly floor(256 / m_cellHeight) rows. The
/// height is a fraction of the screen for most fonts, but a font made for
/// SetTextHeight (flag 0x4, DBM's zooming warnings) has 0.99 with
/// m_pixelsPerUnit 1/64, so only nonsense is refused.
fn header_checks(b: &[u8]) -> bool {
    let (cell, flags, height) = (word(b, 0), word(b, 2), f32::from_bits(word(b, 3)));
    if !(6..=64).contains(&cell) || flags > 0xffff || !(0.0005..64.0).contains(&height) {
        return false;
    }
    let mut live = 0;
    for i in 0..CACHES {
        match cache_rows(b, i, cell) {
            Some(Some(_)) => live += 1,
            Some(None) => {}
            None => return false,
        }
    }
    live > 0
}

/// A CHARCODEDESC from its code word (at the node) to the end of its
/// GLYPHBITMAPDATA, in one read when the link offset is sane: code at 0,
/// the next node's link at linkoffset + 4, the fields from linkoffset + 16.
const DESC_TAIL: u32 = 16 + 56;
const LINKOFFSET_MAX: u32 = 256;

fn parse_font(name_addr: u32, h: u32, cells_too: bool) -> Option<Font> {
    // The header first: most offsets fonts_list_at tries are no header, and
    // that is one read.
    let b = header(h)?;
    if !header_checks(&b) {
        return None;
    }
    let name = name_at(name_addr)?;
    let cell_height = word(&b, 0);
    let flags = word(&b, 2);
    let pixel_size = word(&b, (PIXEL_SIZE / 4) as usize);
    let (requested, pixels_per_unit) = (f32::from_bits(word(&b, 3)), f32::from_bits(word(&b, 4)));
    let mut caches = Vec::new();
    let mut rows = Vec::new();
    let mut desc = [0u8; (LINKOFFSET_MAX + DESC_TAIL) as usize];
    for i in (0..CACHES).filter(|_| cells_too) {
        let texture = word(&b, ((CACHES_AT + i * CACHE_SIZE) / 4) as usize);
        let Some(Some((count, data))) = cache_rows(&b, i, cell_height) else {
            continue;
        };
        // TEXTURECACHEROW (16 bytes): widestFreeSlot, linkoffset, the list's
        // terminator and first node; the rows in one read.
        rows.resize(16 * count as usize, 0);
        if !read_into(data, &mut rows) {
            return None;
        }
        let mut cells = Vec::with_capacity(64);
        for r in 0..count {
            let linkoffset = word(&rows, 4 * r as usize + 1);
            let mut node = word(&rows, 4 * r as usize + 3);
            // A row 256 texels wide holds at most 256 cells: the bound
            // against a cycle, with nothing to allocate.
            let mut left = 256;
            while (node as i32) > 0 && node & 1 == 0 && left > 0 && cells.len() < 4096 {
                left -= 1;
                if linkoffset > LINKOFFSET_MAX {
                    log(&format!(
                        "mem: {} cache {} row {}: link offset {}",
                        name, i, r, linkoffset
                    ));
                    break;
                }
                let d = &mut desc[..(linkoffset + DESC_TAIL) as usize];
                if !read_into(node, d) {
                    return None;
                }
                // The link offset need not be a multiple of 4: by bytes.
                let le = |off: usize| u32::from_le_bytes(d[off..off + 4].try_into().unwrap());
                let f = (linkoffset + 16) as usize;
                let at = |k: usize| le(f + 4 * k);
                let cell = Cell {
                    code: le(0),
                    row: at(1),
                    start: at(2),
                    end: at(3),
                    glyph_width: at(6),
                    glyph_height: at(7),
                    data: at(4),
                    pitch: at(11),
                    y_start: at(13),
                };
                if at(0) != i || cell.row != r || cell.start > cell.end || cell.end > 255 {
                    log(&format!(
                        "mem: odd node {:#x} in {} cache {} row {}",
                        node, name, i, r
                    ));
                    break;
                }
                cells.push(cell);
                node = le(linkoffset as usize + 4);
            }
        }
        caches.push(Cache {
            index: i,
            texture,
            cells,
        });
    }
    Some(Font {
        base: h,
        name,
        cell_height,
        flags,
        pixel_size,
        requested,
        pixels_per_unit,
        caches,
    })
}

/// Header address -> name address of the fonts seen.
fn known() -> &'static Mutex<HashMap<u32, u32>> {
    static K: OnceLock<Mutex<HashMap<u32, u32>>> = OnceLock::new();
    K.get_or_init(Default::default)
}

/// The font whose header is at `h` (m_fontName sits right before it),
/// without its caches' cells: two reads. Remembered for reparse, which reads
/// the cells.
pub fn head_at(h: u32) -> Option<Font> {
    let name = h - NAME_LEN;
    let f = parse_font(name, h, false)?;
    known().lock().unwrap().insert(h, name);
    Some(f)
}

// ---- g_fonts ---------------------------------------------------------
//
// GxuFontCreateFont makes every CGxFont with g_fonts.NewNode(2, ..): linked
// before the list's terminator, i.e. appended (whoa src/gx/Font.cpp, squall
// storm/list/TSList.hpp). CGxFont is a TSLinkedNode, so its TSLink
// (m_prevlink, m_next) is the object's first two words, and the list holds
// every font, SetTextHeight's included.
//
// g_fonts is a static in Wow.exe, found once by what makes it a list of
// fonts, not by its address: a word pair (m_prevlink, m_next) whose first
// node links back to it, whose last node's m_next names it as the end, and
// whose first node is a CGxFont (a header that checks out at a fixed
// distance, 376 bytes in 12340). Storm tags the end in the last m_next: the
// 3.3.5a client sets the low bit of the terminator's address (seen in a
// dump; squall writes its complement instead, which this client does not).

/// The list as found: its terminator and how far past a CGxFont its header
/// sits.
#[derive(Clone, Copy)]
struct Registry {
    terminator: u32,
    header_offset: u32,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);
/// When the image was last searched without a find (no font made yet).
static LAST_SEARCH: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const SEARCH_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
/// More fonts than any client makes: the bound against a cycle.
const FONTS_MAX: usize = 4096;
/// Where a header may sit past its CGxFont: after the name at least.
const HEADER_OFFSETS: std::ops::RangeInclusive<u32> = NAME_LEN..=NAME_LEN + 256;

/// IMAGE_SCN_MEM_WRITE: g_fonts changes, so it lies in such a section.
const SECTION_WRITE: u32 = 0x8000_0000;

/// Wow.exe's image, [base, end), and its writable sections' ranges.
fn exe_image() -> Option<((u32, u32), Vec<(u32, u32)>)> {
    let base = unsafe { GetModuleHandleA(std::ptr::null()) } as usize as u32;
    let nt = base.checked_add(u32_at(base.checked_add(0x3c)?)?)?;
    // FILE_HEADER at nt + 4: NumberOfSections at + 2, SizeOfOptionalHeader
    // at + 16; the optional header (SizeOfImage at + 56) and then the
    // section table (40 bytes each: VirtualSize + 8, VirtualAddress + 12,
    // Characteristics + 36).
    let head = read(nt, 24)?;
    let sections = u16::from_le_bytes([head[6], head[7]]) as usize;
    let optional = u16::from_le_bytes([head[20], head[21]]) as u32;
    let size = u32_at(nt.checked_add(24 + 56)?)?;
    let table = read(nt.checked_add(24 + optional)?, 40 * sections)?;
    let writable = (0..sections)
        .map(|s| &table[40 * s..40 * s + 40])
        .filter(|s| word(s, 9) & SECTION_WRITE != 0)
        .filter_map(|s| {
            let start = base.checked_add(word(s, 3))?;
            Some((start, start.checked_add(word(s, 2))?))
        })
        .collect();
    Some(((base, base.checked_add(size)?), writable))
}

/// The terminator a link's m_next names if it ends the list: the client
/// tags it with the low bit (&m_terminator | 1). None for a node.
fn end(next: u32) -> Option<u32> {
    (next & 1 != 0).then_some(next ^ 1)
}

/// Whether the terminator at `t` (in the image, holding m_prevlink `prev`
/// and m_next `first`) ends a list of CGxFonts; the header offset if so.
fn fonts_list_at(t: u32, prev: u32, first: u32, image: (u32, u32)) -> Option<u32> {
    let heap = |p: u32| p >= 0x10000 && p & 3 == 0 && !(image.0..image.1).contains(&p);
    if !heap(prev) || !heap(first) {
        return None;
    }
    // The first node links back here, the last names this as its end.
    if u32_at(first)? != t || end(u32_at(prev.checked_add(4)?)?) != Some(t) {
        return None;
    }
    // A font with a live cache somewhere along it (the oldest ones may have
    // none), each node linked back to the one before.
    let (mut back, mut node) = (t, first);
    for _ in 0..FONTS_MAX {
        if end(node).is_some() || u32_at(node)? != back {
            return None;
        }
        if let Some(o) = HEADER_OFFSETS
            .step_by(4)
            .find(|&o| node.checked_add(o).and_then(head_at).is_some())
        {
            return Some(o);
        }
        (back, node) = (node, u32_at(node.checked_add(4)?)?);
    }
    None
}

/// Search Wow.exe's writable sections for g_fonts: every aligned word
/// pair, a page at a time; the cheap tests on the words come first.
fn search() -> Option<Registry> {
    let (image, writable) = exe_image()?;
    let mut page = [0u8; 4096];
    for (start, stop) in writable {
        let mut at = start & !4095;
        while at < stop {
            if read_into(at, &mut page) {
                for i in 0..page.len() / 4 - 1 {
                    let (prev, first) = (word(&page, i), word(&page, i + 1));
                    let t = at + 4 * i as u32;
                    if let Some(header_offset) = fonts_list_at(t, prev, first, image) {
                        return Some(Registry {
                            terminator: t,
                            header_offset,
                        });
                    }
                }
            }
            at += page.len() as u32;
        }
    }
    None
}

/// Find g_fonts if not yet (a search at most every SEARCH_EVERY: before
/// the client makes its first font there is nothing to find).
fn ensure_registry() {
    if REGISTRY.lock().unwrap().is_some() {
        return;
    }
    {
        let mut last = LAST_SEARCH.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < SEARCH_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let t = std::time::Instant::now();
    let Some(r) = search() else {
        log(&format!(
            "mem: g_fonts not found in the image ({} ms)",
            t.elapsed().as_millis()
        ));
        return;
    };
    *REGISTRY.lock().unwrap() = Some(r);
    let fonts = registry_headers().unwrap_or_default();
    let linkoffset = u32_at(r.terminator - 4).unwrap_or(u32::MAX);
    log(&format!(
        "mem: g_fonts terminator {:#x} (linkoffset {}), header at CGxFont + {}, {} fonts ({} ms)",
        r.terminator,
        linkoffset,
        r.header_offset,
        fonts.len(),
        t.elapsed().as_millis()
    ));
}

/// From one read of the header at `h`, if it still checks out: its cell
/// height, its flags and the HTEXTURE in texture cache `i`. What an owner
/// remembered is still true when these are.
pub fn slot(h: u32, i: u32) -> Option<(u32, u32, u32)> {
    let b = header(h)?;
    header_checks(&b).then(|| {
        (
            word(&b, 0),
            word(&b, 2),
            word(&b, ((CACHES_AT + i * CACHE_SIZE) / 4) as usize),
        )
    })
}

/// Whether g_fonts has been found.
pub fn registry_found() -> bool {
    REGISTRY.lock().unwrap().is_some()
}

/// The header of every CGxFont the client has, in creation order; None
/// until the list is found.
fn registry_headers() -> Option<Vec<u32>> {
    let r = (*REGISTRY.lock().unwrap())?;
    let mut node = u32_at(r.terminator + 4)?;
    let mut out = Vec::new();
    while end(node).is_none() && out.len() < FONTS_MAX {
        out.push(node.checked_add(r.header_offset)?);
        node = u32_at(node.checked_add(4)?)?;
    }
    Some(out)
}

/// The HTEXTURE of each texture cache (0 = none) of every font the client
/// has (g_fonts, found on the first call) whose header checks out, one read
/// per font. A freed font's memory may still look like one, so only the
/// list counts.
pub fn known_textures() -> Vec<(u32, [u32; CACHES as usize])> {
    ensure_registry();
    let headers = registry_headers().unwrap_or_default();
    headers
        .into_iter()
        .filter_map(|h| {
            let b = header(h)?;
            header_checks(&b).then(|| {
                (
                    h,
                    std::array::from_fn(|i| {
                        word(&b, ((CACHES_AT + i as u32 * CACHE_SIZE) / 4) as usize)
                    }),
                )
            })
        })
        .collect()
}

/// Re-read a font seen before: the name, the header, each cache's rows and
/// each cell, one read each.
pub fn reparse(header: u32) -> Option<Font> {
    let name = *known().lock().unwrap().get(&header)?;
    parse_font(name, header, true)
}
