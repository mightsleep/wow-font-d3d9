//! Glyph cache textures. The client fills a cache through its level 0
//! surface (GetSurfaceLevel, LockRect, UnlockRect), so the texture and the
//! surface both get hooked vtables. At UnlockRect the texels are compared
//! with a shadow of the last upload, and the payload takes the changed
//! cells (it never writes the client's texture, so the shadow is it).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::{Class, HRESULT, Ptr, log, payload};

const TEX_GET_SURFACE_LEVEL: usize = 18;
const TEX_LOCK_RECT: usize = 19;
const TEX_UNLOCK_RECT: usize = 20;
const SURF_LOCK_RECT: usize = 13;
const SURF_UNLOCK_RECT: usize = 14;
/// Entries copied; the interfaces have 22 and 17 (see DEV_VTABLE on why
/// more).
const VTABLE: usize = 64;

/// Glyph caches, by their vtable pointer (lib::Class): phi tells them from
/// other textures in one compare.
pub static TEX: Class = Class::new();
/// Their level 0 surfaces.
static SURF: Class = Class::new();

#[repr(C)]
struct LockedRect {
    pitch: i32,
    bits: Ptr,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

const FULL: Rect = Rect {
    left: 0,
    top: 0,
    right: 256,
    bottom: 256,
};

struct Cache {
    id: usize,
    /// What the texture held after the last unlock, as far as we saw.
    shadow: Vec<u16>,
    /// Texels the client changed since the payload last went over the
    /// texture, a bit each (payload::Dirty).
    dirty: payload::Dirty,
    /// The current level 0 lock: bits, pitch, rectangle.
    lock: Option<(usize, i32, Rect)>,
    /// settings::generation() when the texture last got our cells.
    generation: u64,
}

#[derive(Default)]
struct State {
    /// Texture -> cache.
    caches: HashMap<usize, Cache>,
    /// Level 0 surface -> its texture.
    surfaces: HashMap<usize, usize>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn tex_orig(i: usize) -> usize {
    TEX.orig(i)
}

fn surf_orig(i: usize) -> usize {
    SURF.orig(i)
}

/// Hook a new glyph cache.
pub fn hook(tex: Ptr, id: usize) {
    let hooks = [
        (
            TEX_GET_SURFACE_LEVEL,
            get_surface_level as *const () as usize,
        ),
        (TEX_LOCK_RECT, tex_lock_rect as *const () as usize),
        (TEX_UNLOCK_RECT, tex_unlock_rect as *const () as usize),
    ];
    if !unsafe { TEX.adopt(tex, VTABLE, &hooks) } {
        log(&format!(
            "img{}: a texture of another class, left alone",
            id
        ));
        return;
    }
    crate::fonts::forget(tex as usize);
    crate::v2::forget(tex as usize);
    let mut s = state().lock().unwrap();
    s.surfaces.retain(|_, t| *t != tex as usize);
    s.caches.insert(
        tex as usize,
        Cache {
            id,
            shadow: vec![0; 256 * 256],
            dirty: payload::Dirty::all(),
            lock: None,
            generation: crate::settings::generation(),
        },
    );
}

/// A texture at this address is no glyph cache (any more).
pub fn forget(tex: usize) {
    let mut s = state().lock().unwrap();
    s.caches.remove(&tex);
    s.surfaces.retain(|_, t| *t != tex);
    // Not under our lock: no lock is held while another is taken.
    drop(s);
    crate::fonts::forget(tex);
    crate::v2::forget(tex);
}

/// Note a level 0 lock of `tex`'s cache.
fn begin(tex: usize, locked: *const LockedRect, rect: *const Rect) {
    // Who is uploading: from our own stack, while the client is below us.
    if crate::enabled() {
        crate::fonts::identify(tex);
    }
    let area = if rect.is_null() {
        FULL
    } else {
        unsafe { *rect }
    };
    let l = unsafe { &*locked };
    if let Some(c) = state().lock().unwrap().caches.get_mut(&tex) {
        c.lock = Some((l.bits as usize, l.pitch, area));
    }
}

/// Compare what the client wrote with the shadow, then hand the payload
/// the new shadow.
fn end(tex: usize) {
    let mut s = state().lock().unwrap();
    let Some(c) = s.caches.get_mut(&tex) else {
        return;
    };
    let Some((bits, pitch, a)) = c.lock.take() else {
        return;
    };
    let trace = crate::trace::span(crate::trace::Kind::Diff);
    for y in a.top.max(0)..a.bottom.min(256) {
        let row = (bits as isize + (y - a.top) as isize * pitch as isize) as *const u16;
        for x in a.left.max(0)..a.right.min(256) {
            let v = unsafe { row.add((x - a.left) as usize).read_unaligned() };
            let sh = &mut c.shadow[(y * 256 + x) as usize];
            if *sh != v {
                *sh = v;
                c.dirty.set(x as u32, y as u32);
            }
        }
    }
    drop(trace);
    // The payload goes over it only when the whole texture is locked (the
    // only way the client does it).
    let full = a.left == 0 && a.top == 0 && a.right == 256 && a.bottom == 256;
    if !crate::enabled() || !full {
        return;
    }
    let dirty = std::mem::replace(&mut c.dirty, payload::Dirty::none());
    let (id, shadow) = (c.id, c.shadow.clone());
    drop(s);
    let summary = {
        let _trace = crate::trace::span(crate::trace::Kind::Payload);
        payload::write(tex, &shadow, &dirty).1
    };
    // Logged when it differs from the last one for this cache.
    static LAST: OnceLock<Mutex<HashMap<usize, String>>> = OnceLock::new();
    let mut last = LAST.get_or_init(Default::default).lock().unwrap();
    if last.get(&tex) != Some(&summary) {
        log(&format!("img{} payload: {}", id, summary));
        last.insert(tex, summary);
    }
}

unsafe extern "system" fn tex_lock_rect(
    this: Ptr,
    level: u32,
    locked: *mut LockedRect,
    rect: *const Rect,
    flags: u32,
) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, u32, *mut LockedRect, *const Rect, u32) -> HRESULT =
        unsafe { std::mem::transmute(tex_orig(TEX_LOCK_RECT)) };
    let r = unsafe { f(this, level, locked, rect, flags) };
    if r >= 0 && level == 0 && !locked.is_null() {
        begin(this as usize, locked, rect);
    }
    r
}

unsafe extern "system" fn tex_unlock_rect(this: Ptr, level: u32) -> HRESULT {
    if level == 0 {
        end(this as usize);
    }
    let f: unsafe extern "system" fn(Ptr, u32) -> HRESULT =
        unsafe { std::mem::transmute(tex_orig(TEX_UNLOCK_RECT)) };
    unsafe { f(this, level) }
}

unsafe extern "system" fn surf_lock_rect(
    this: Ptr,
    locked: *mut LockedRect,
    rect: *const Rect,
    flags: u32,
) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, *mut LockedRect, *const Rect, u32) -> HRESULT =
        unsafe { std::mem::transmute(surf_orig(SURF_LOCK_RECT)) };
    let r = unsafe { f(this, locked, rect, flags) };
    let tex = state()
        .lock()
        .unwrap()
        .surfaces
        .get(&(this as usize))
        .copied();
    if r >= 0
        && !locked.is_null()
        && let Some(tex) = tex
    {
        begin(tex, locked, rect);
    }
    r
}

unsafe extern "system" fn surf_unlock_rect(this: Ptr) -> HRESULT {
    let tex = state()
        .lock()
        .unwrap()
        .surfaces
        .get(&(this as usize))
        .copied();
    if let Some(tex) = tex {
        end(tex);
    }
    let f: unsafe extern "system" fn(Ptr) -> HRESULT =
        unsafe { std::mem::transmute(surf_orig(SURF_UNLOCK_RECT)) };
    unsafe { f(this) }
}

/// The client locks through the level 0 surface: hook it and remember whose
/// it is.
unsafe extern "system" fn get_surface_level(this: Ptr, level: u32, out: *mut Ptr) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, u32, *mut Ptr) -> HRESULT =
        unsafe { std::mem::transmute(tex_orig(TEX_GET_SURFACE_LEVEL)) };
    let r = unsafe { f(this, level, out) };
    if r < 0 || level != 0 || out.is_null() || unsafe { (*out).is_null() } {
        return r;
    }
    let surf = unsafe { *out };
    let id = {
        let mut s = state().lock().unwrap();
        let Some(id) = s.caches.get(&(this as usize)).map(|c| c.id) else {
            return r;
        };
        // Known only if it is this texture's and still ours: a freed cache's
        // surface stays in the map until its texture's address comes back,
        // and a new cache's surface at that address went unhooked, its
        // glyphs bitmaps for good (MSBT crits, img39).
        let known = s.surfaces.insert(surf as usize, this as usize) == Some(this as usize);
        if known && SURF.has(surf) {
            return r;
        }
        id
    };
    let hooks = [
        (SURF_LOCK_RECT, surf_lock_rect as *const () as usize),
        (SURF_UNLOCK_RECT, surf_unlock_rect as *const () as usize),
    ];
    if !unsafe { SURF.adopt(surf, VTABLE, &hooks) } {
        log(&format!(
            "img{}: a surface of another class, left alone",
            id
        ));
        return r;
    }
    log(&format!(
        "img{} level 0 surface {:#x} hooked",
        id, surf as usize
    ));
    r
}

/// Every glyph cache: (texture, id).
pub fn ids() -> Vec<(usize, usize)> {
    state()
        .lock()
        .unwrap()
        .caches
        .iter()
        .map(|(&t, c)| (t, c.id))
        .collect()
}

/// Whether `tex` got our cells only before the last change of a setting
/// they depend on.
pub fn stale(tex: usize) -> bool {
    state()
        .lock()
        .unwrap()
        .caches
        .get(&tex)
        .is_some_and(|c| c.generation != crate::settings::generation())
}

/// Put `tex`'s cells into v2 from the client's last upload (the shadow,
/// which is the texture: nothing of ours goes into it): when the client
/// uploaded before its font was known (it will not upload again until a
/// new glyph comes) or after a setting changed.
pub fn refresh(tex: usize) {
    let _trace = crate::trace::span(crate::trace::Kind::Refresh);
    let Some(header) = crate::fonts::owners()
        .lock()
        .unwrap()
        .get(&tex)
        .map(|o| o.header)
    else {
        return;
    };
    let (id, shadow, dirty) = {
        let mut s = state().lock().unwrap();
        let Some(c) = s.caches.get_mut(&tex) else {
            return;
        };
        c.generation = crate::settings::generation();
        (
            c.id,
            c.shadow.clone(),
            std::mem::replace(&mut c.dirty, payload::Dirty::none()),
        )
    };
    payload::tried(tex, header);
    let (written, summary) = payload::write(tex, &shadow, &dirty);
    // Nothing went in (an owner with no cells of this texture: MSBT
    // animating a crit's size makes a new font each frame, its table
    // empty): the changes stay to be checked by the next pass.
    if written == 0
        && let Some(c) = state().lock().unwrap().caches.get_mut(&tex)
    {
        c.dirty = dirty;
    }
    log(&format!("img{} refresh: {}", id, summary));
}
