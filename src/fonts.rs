//! Which font each glyph cache belongs to. A CGxFont's texture cache holds
//! an HTEXTURE; HTEXTURE + 0x44 points at the client's CGxTex and CGxTex +
//! 0x38 at the IDirect3DTexture9 we saw created (3.3.5a, 12340).
//!
//! No search: the client keeps every font in its own list (mem's g_fonts),
//! so the owner of a texture is the font one of whose caches leads to it.
//! It is asked at every upload (identify), so a recycled texture names its
//! new font; one that stops checking out is dropped (lost) meanwhile.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::{log, mem, texture};

/// HTEXTURE -> CGxTex -> IDirect3DTexture9 in the 3.3.5a client.
const CHAIN: [u32; 2] = [0x44, 0x38];

/// What the proxy knows about one glyph cache.
#[derive(Clone, PartialEq)]
pub struct Owner {
    pub name: String,
    pub pixel_size: u32,
    /// The height asked for in screen pixels (mem::Font::requested times
    /// pixels_per_unit), before the client's cap.
    pub requested_px: f32,
    pub cell_height: u32,
    pub flags: u32,
    pub cache: u32,
    /// CGxFont header (mem::reparse).
    pub header: u32,
    /// The HTEXTURE in that cache: while the cache still holds it, the
    /// texture behind it is the same (identify checks this one word).
    pub handle: u32,
}

/// Glyph cache texture -> its font.
pub fn owners() -> &'static Mutex<HashMap<usize, Owner>> {
    static O: OnceLock<Mutex<HashMap<usize, Owner>>> = OnceLock::new();
    O.get_or_init(Default::default)
}

/// A glyph cache's owner turned out wrong: forget it; the next upload of
/// the texture names the new one.
pub fn lost(tex: usize) {
    owners().lock().unwrap().remove(&tex);
}

/// Follow `offsets` from `handle`: each step reads the word at the current
/// address plus the offset, the last word is the result.
fn follow(handle: u32, offsets: &[u32]) -> Option<u32> {
    let mut a = handle;
    for &o in offsets {
        a = mem::u32_at(a.checked_add(o)?)?;
    }
    Some(a)
}

/// The IDirect3DTexture9 behind an HTEXTURE.
pub fn texture_of(handle: u32) -> Option<u32> {
    follow(handle, &CHAIN)
}

fn owner_of(f: mem::Font, cache: u32, handle: u32) -> Owner {
    Owner {
        name: f.name,
        pixel_size: f.pixel_size,
        requested_px: f.requested * f.pixels_per_unit,
        cell_height: f.cell_height,
        flags: f.flags,
        cache,
        header: f.base,
        handle,
    }
}

/// The owner of `tex` from the client's own list of fonts (mem's g_fonts):
/// the font one of whose texture caches holds an HTEXTURE leading to it.
/// A walk over every font, for a texture not seen before or one whose
/// owner stopped holding it.
fn registry_owner(tex: usize) -> Option<Owner> {
    mem::known_textures()
        .into_iter()
        .find_map(|(header, handles)| {
            handles.iter().enumerate().find_map(|(i, &handle)| {
                (handle >= 0x10000 && texture_of(handle) == Some(tex as u32))
                    .then(|| mem::head_at(header).map(|f| owner_of(f, i as u32, handle)))
                    .flatten()
            })
        })
}

/// Whether the owner remembered for `tex` still is: its font's header
/// checks out with the same cell height and flags, and the cache still
/// holds the same HTEXTURE. One read.
fn still_owner(tex: usize) -> bool {
    let known = owners()
        .lock()
        .unwrap()
        .get(&tex)
        .map(|o| (o.header, o.cache, (o.cell_height, o.flags, o.handle)));
    known.is_some_and(|(header, cache, want)| mem::slot(header, cache) == Some(want))
}

/// Glyph caches whose upload found no owner (a new cache filled before the
/// client stored its HTEXTURE in m_texture, seen with DBM's warning fonts),
/// with when adoption was last tried. Such a cache may never be uploaded
/// again, so it is adopted at a draw (adopt).
fn orphans() -> &'static Mutex<HashMap<usize, Option<Instant>>> {
    static O: OnceLock<Mutex<HashMap<usize, Option<Instant>>>> = OnceLock::new();
    O.get_or_init(Default::default)
}

/// Caches already logged as drawn with no font known.
static DRAWN_ORPHANS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// What was logged about the texture that had this address before: a new
/// one there gets its own lines.
pub fn forget(tex: usize) {
    orphans().lock().unwrap().remove(&tex);
    DRAWN_ORPHANS.lock().unwrap().retain(|&t| t != tex);
}

fn img(tex: usize) -> usize {
    texture::ids()
        .into_iter()
        .find(|&(t, _)| t == tex)
        .map_or(usize::MAX, |(_, id)| id)
}

/// An upload of `tex` found no owner: logged once per texture.
fn orphaned(tex: usize) {
    if orphans().lock().unwrap().insert(tex, None).is_some() {
        return;
    }
    log(&format!(
        "fonts: img{} upload with no owner in g_fonts ({})",
        img(tex),
        if mem::registry_found() {
            "list found"
        } else {
            "list not found"
        }
    ));
}

/// A glyph cache drawn while its font is unknown (stays bitmap), once each.
pub fn drawn_orphan(tex: usize) {
    // Not under our lock: img asks texture's, and no lock is held while
    // another is taken (bar the log's, a leaf).
    let new = {
        let mut s = DRAWN_ORPHANS.lock().unwrap();
        let new = !s.contains(&tex);
        if new {
            s.push(tex);
        }
        new
    };
    if new {
        log(&format!(
            "fonts: img{} drawn with no font known (bitmap)",
            img(tex)
        ));
    }
}

/// Give orphaned caches their font, at most once a second per cache
/// (`now` = whatever the time).
pub fn adopt(now: bool) {
    let due: Vec<usize> = {
        let mut o = orphans().lock().unwrap();
        o.iter_mut()
            .filter(|(_, t)| now || t.is_none_or(|t| t.elapsed().as_secs() >= 1))
            .map(|(&tex, t)| {
                *t = Some(Instant::now());
                tex
            })
            .collect()
    };
    for tex in due {
        let Some(owner) = registry_owner(tex) else {
            continue;
        };
        log(&format!(
            "fonts: img{} <- {} px={} cell={} flags={:#x} cache {} (adopted at a draw)",
            img(tex),
            owner.name,
            owner.pixel_size,
            owner.cell_height,
            owner.flags,
            owner.cache
        ));
        owners().lock().unwrap().insert(tex, owner);
        orphans().lock().unwrap().remove(&tex);
    }
}

/// Uploads whose owner was verified (one read), and their time, for the
/// log.
static VERIFIED: AtomicUsize = AtomicUsize::new(0);
static VERIFIED_NS: AtomicUsize = AtomicUsize::new(0);

fn verified() -> String {
    let n = VERIFIED.load(Ordering::Relaxed);
    let ns = VERIFIED_NS.load(Ordering::Relaxed);
    format!("{n} verified so far, {} ns each", ns / n.max(1))
}

/// Who owns glyph cache `tex`, asked from inside its LockRect (the client
/// is about to upload into it, so its font has it by now): the owner known
/// if it still is, else a walk of the client's fonts.
pub fn identify(tex: usize) -> bool {
    let _trace = crate::trace::span(crate::trace::Kind::Identify);
    let t = Instant::now();
    if still_owner(tex) {
        VERIFIED.fetch_add(1, Ordering::Relaxed);
        VERIFIED_NS.fetch_add(t.elapsed().as_nanos() as usize, Ordering::Relaxed);
        return true;
    }
    let Some(owner) = registry_owner(tex) else {
        if !owners().lock().unwrap().contains_key(&tex) {
            orphaned(tex);
        }
        return false;
    };
    orphans().lock().unwrap().remove(&tex);
    // The owner is stored (its name cloned) only when it changed.
    let changed = {
        let mut o = owners().lock().unwrap();
        let changed = o.get(&tex) != Some(&owner);
        if changed {
            o.insert(tex, owner.clone());
        }
        changed
    };
    if changed {
        log(&format!(
            "fonts: img{} <- {} px={} (requested {:.2}) cell={} flags={:#x} cache {} header {:#x} (walk {} us; {})",
            img(tex),
            owner.name,
            owner.pixel_size,
            owner.requested_px,
            owner.cell_height,
            owner.flags,
            owner.cache,
            owner.header,
            t.elapsed().as_micros(),
            verified()
        ));
    }
    true
}
