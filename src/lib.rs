//! Proxy d3d9.dll. Sits next to Wow.exe, loads the real d3d9.dll
//! ($WOW_FONT_D3D9_REAL, else the one in the system directory: Windows'
//! own, or DXVK or wined3d under Wine) and forwards everything. Unless
//! WOW_FONT_D3D9=0 it writes vector glyphs into the client's glyph caches
//! (256x256 A4R4G4B4 textures; texture, payload) and has them drawn by the
//! decoder (phi, draw).
//!
//! Hooks point an object at a patched copy of its class's vtable (Class);
//! no code anywhere is patched. Log: wow-font-d3d9.log next to Wow.exe.

#![allow(non_snake_case)]

#[allow(dead_code)]
mod atlas;
mod cache;
mod draw;
mod fontfile;
#[allow(dead_code)]
mod glyph;
#[allow(dead_code)]
mod coverage;
mod fonts;
mod mem;
mod mpq;
mod payload;
mod record;
mod phi;
mod settings;
mod texture;
mod trace;
mod v2;

use std::ffi::c_void;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

pub(crate) type HRESULT = i32;
pub(crate) type Ptr = *mut c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    pub(crate) fn LoadLibraryA(name: *const u8) -> Ptr;
    pub(crate) fn GetProcAddress(module: Ptr, name: *const u8) -> Ptr;
    fn GetSystemDirectoryA(buf: *mut u8, size: u32) -> u32;
    fn GetModuleFileNameA(module: Ptr, buf: *mut u8, size: u32) -> u32;
    fn GetModuleHandleExA(flags: u32, name: *const u8, module: *mut Ptr) -> i32;
}

const D3DFMT_A4R4G4B4: u32 = 26;
const D3DERR_NOTAVAILABLE: HRESULT = 0x8876086Au32 as i32;

/// The folder of Wow.exe (the launcher's working directory can be anything).
pub(crate) fn exe_dir() -> &'static str {
    static D: OnceLock<String> = OnceLock::new();
    D.get_or_init(|| {
        let mut buf = [0u8; 260];
        let n = unsafe { GetModuleFileNameA(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let exe = String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned();
        exe.rfind('\\').map_or(String::new(), |i| exe[..i].to_string())
    })
}

/// A file next to Wow.exe.
pub(crate) fn beside_exe(name: &str) -> String {
    match exe_dir() {
        "" => name.to_string(),
        d => format!("{}\\{}", d, name),
    }
}

pub(crate) fn log(msg: &str) {
    write_log(msg, true);
}

/// A line only if the log is free right now: for the panic hook and the
/// process's end, where whoever holds it may never let go (a thread
/// killed at exit, or the panicking thread itself, inside log).
fn log_nowait(msg: &str) {
    write_log(msg, false);
}

/// The log's lock is a leaf: nothing is locked while it is held, so taking
/// it under any other lock cannot deadlock.
fn write_log(msg: &str, wait: bool) {
    static F: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
    let f = F.get_or_init(|| {
        let path = beside_exe("wow-font-d3d9.log");
        Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(path).ok())
    });
    // UTC time of day, to the millisecond: lines from different runs and
    // other tools line up.
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() % 86_400_000);
    let mut guard = if wait {
        f.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match f.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        }
    };
    if let Some(f) = guard.as_mut() {
        let _ = writeln!(
            f,
            "{:02}:{:02}:{:02}.{:03} {}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000,
            msg
        );
    }
}

pub(crate) fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var("WOW_FONT_D3D9").map_or(true, |v| v != "0"))
}

/// The real d3d9.dll, loaded once.
fn real() -> Ptr {
    static R: OnceLock<usize> = OnceLock::new();
    *R.get_or_init(|| {
        let path = std::env::var("WOW_FONT_D3D9_REAL").unwrap_or_else(|_| {
            let mut buf = [0u8; 260];
            let n = unsafe { GetSystemDirectoryA(buf.as_mut_ptr(), buf.len() as u32) } as usize;
            format!(
                "{}\\d3d9.dll",
                String::from_utf8_lossy(&buf[..n.min(buf.len())])
            )
        });
        let c = std::ffi::CString::new(path.clone()).unwrap_or_default();
        let h = unsafe { LoadLibraryA(c.as_ptr() as *const u8) };
        log(&format!(
            "real d3d9: {} -> {:?}; glyph cache hooks {}",
            path,
            h,
            if enabled() { "on" } else { "off" }
        ));
        h as usize
    }) as Ptr
}

/// A function of the real d3d9.dll by NUL-terminated name, or null.
fn real_fn(name: &[u8]) -> Ptr {
    let m = real();
    if m.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { GetProcAddress(m, name.as_ptr()) }
}

// ---- loading ----------------------------------------------------------

const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 1;
const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 4;

/// Pin ourselves on load: the client probes D3D9, frees the library and
/// loads it again. Unloaded, we would leave the real d3d9.dll behind under
/// the same module name, and the second LoadLibrary("d3d9.dll") would find
/// that one already loaded and never reach us.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(_module: Ptr, reason: u32, _reserved: Ptr) -> i32 {
    match reason {
        1 => {
            let mut m: Ptr = std::ptr::null_mut();
            let pinned = unsafe {
                GetModuleHandleExA(
                    GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                    DllMain as *const () as *const u8,
                    &mut m,
                )
            };
            log(&format!("proxy attached, pinned: {}", pinned != 0));
            // panic = "abort" takes the game down without a word: say where.
            std::panic::set_hook(Box::new(|info| log_nowait(&format!("PANIC: {info}"))));
        }
        // Pinned, so only at the process's end, after its other threads
        // (ours too) were killed wherever they were.
        0 => log_nowait("proxy detached"),
        _ => {}
    }
    1
}

// ---- exports ----------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Direct3DCreate9(sdk: u32) -> Ptr {
    let f = real_fn(b"Direct3DCreate9\0");
    if f.is_null() {
        log("Direct3DCreate9: real function missing");
        return std::ptr::null_mut();
    }
    let f: unsafe extern "system" fn(u32) -> Ptr = unsafe { std::mem::transmute(f) };
    let d3d = unsafe { f(sdk) };
    log(&format!("Direct3DCreate9({}) -> {:?}", sdk, d3d));
    if enabled() && !d3d.is_null() {
        unsafe { hook_d3d(d3d) };
    }
    d3d
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Direct3DCreate9Ex(sdk: u32, out: *mut Ptr) -> HRESULT {
    let f = real_fn(b"Direct3DCreate9Ex\0");
    if f.is_null() {
        return D3DERR_NOTAVAILABLE;
    }
    let f: unsafe extern "system" fn(u32, *mut Ptr) -> HRESULT = unsafe { std::mem::transmute(f) };
    let r = unsafe { f(sdk, out) };
    log(&format!("Direct3DCreate9Ex({}) -> {:#x}", sdk, r));
    if enabled() && r >= 0 && !out.is_null() && unsafe { !(*out).is_null() } {
        unsafe { hook_d3d(*out) };
    }
    r
}

/// Forward an export, or return `$default` when the real DLL lacks it.
macro_rules! forward {
    ($name:ident ( $($a:ident : $t:ty),* ) -> $r:ty = $default:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn $name($($a: $t),*) -> $r {
            let f = real_fn(concat!(stringify!($name), "\0").as_bytes());
            if f.is_null() {
                return $default;
            }
            let f: unsafe extern "system" fn($($t),*) -> $r = unsafe { std::mem::transmute(f) };
            unsafe { f($($a),*) }
        }
    };
}

forward!(D3DPERF_BeginEvent(col: u32, name: *const u16) -> i32 = 0);
forward!(D3DPERF_EndEvent() -> i32 = 0);
forward!(D3DPERF_GetStatus() -> u32 = 0);
forward!(D3DPERF_QueryRepeatFrame() -> i32 = 0);
forward!(D3DPERF_SetMarker(col: u32, name: *const u16) -> () = ());
forward!(D3DPERF_SetOptions(options: u32) -> () = ());
forward!(D3DPERF_SetRegion(col: u32, name: *const u16) -> () = ());
forward!(Direct3DShaderValidatorCreate9() -> Ptr = std::ptr::null_mut());

// ---- vtable hooks -----------------------------------------------------
//
// A hooked object points at a patched copy of its class's vtable, one copy
// per class. So the vtable pointer tags our objects (Class::has is one
// compare); a destructor first points the object back at its class's own
// vtable, so tagged implies alive; and hooks call the original through the
// live vtable (Class::orig), so a tool that patches it later (an overlay,
// ReShade) still gets our calls.
//
// The copies live in one arena aligned to its size, 2^SLOT_SHIFT bytes a
// slot: "ours?" is a mask and a compare, the class a shift (Class::of). A
// slot holds the original vtable's address, the two words before a vtable
// (offset to top and RTTI, or MSVC's object locator), then the entries.

const SLOT_SHIFT: u32 = 11;
const SLOTS: usize = 8;
const ARENA: usize = SLOTS << SLOT_SHIFT;
/// Words before a copy's first entry: the original vtable, offset to top,
/// RTTI.
const HEAD: usize = 3;
const ENTRIES_MAX: usize = (1 << SLOT_SHIFT) / 4 - HEAD;

/// The arena, zeroed and aligned to its size (an allocation: a static's
/// alignment past its section's 4 KiB is not kept by the loader).
fn arena() -> usize {
    static A: OnceLock<usize> = OnceLock::new();
    *A.get_or_init(|| {
        let layout = std::alloc::Layout::from_size_align(ARENA, ARENA).unwrap();
        let p = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!p.is_null(), "vtable arena");
        p as usize
    })
}

static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// One patched copy of a class's vtable (see above).
pub(crate) struct Class {
    /// The class's vtable and our copy of it.
    table: OnceLock<(usize, usize)>,
}

impl Class {
    pub(crate) const fn new() -> Class {
        Class { table: OnceLock::new() }
    }

    /// Point `obj` at the copy, made from the first object's vtable with
    /// `n` entries and `hooks` patched in. False if `obj` is of another
    /// class, or tagged for another Class (left alone).
    pub(crate) unsafe fn adopt(&self, obj: Ptr, n: usize, hooks: &[(usize, usize)]) -> bool {
        unsafe {
            let vt = *(obj as *const usize);
            let &(theirs, ours) = self.table.get_or_init(|| {
                let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
                assert!(slot < SLOTS && n <= ENTRIES_MAX, "vtable arena too small");
                let p = (arena() + (slot << SLOT_SHIFT)) as *mut usize;
                let from = vt as *const usize;
                *p = vt;
                for i in 0..2 + n {
                    *p.add(1 + i) = *from.add(i).sub(2);
                }
                for &(i, f) in hooks {
                    *p.add(HEAD + i) = f;
                }
                (vt, p.add(HEAD) as usize)
            });
            if vt == ours {
                return true;
            }
            if vt != theirs || of(vt).is_some() {
                return false;
            }
            *(obj as *mut usize) = ours;
            true
        }
    }

    /// Whether `obj` points at our copy.
    pub(crate) fn has(&self, obj: Ptr) -> bool {
        !obj.is_null() && self.table.get().is_some_and(|t| unsafe { *(obj as *const usize) } == t.1)
    }

    /// The class's own method `i`, read from its vtable at the call.
    pub(crate) fn orig(&self, i: usize) -> usize {
        let theirs = self.table.get().expect("class not hooked").0;
        unsafe { *(theirs as *const usize).add(i) }
    }
}

/// Which arena slot vtable pointer `vt` is, if it is one of our copies.
pub(crate) fn of(vt: usize) -> Option<usize> {
    let base = arena();
    (vt & !(ARENA - 1) == base).then(|| (vt - base) >> SLOT_SHIFT)
}

static D3D: Class = Class::new();
static DEVICE: Class = Class::new();

const D3D_CREATE_DEVICE: usize = 16;
/// Entries copied: more than the interfaces have (IDirect3D9Ex 22,
/// IDirect3DDevice9Ex 134). The class's vtable goes on past the interface:
/// DXVK implements the Ex classes and calls its own methods virtually
/// (CreateDevice goes through CreateDeviceEx), and being built by MinGW its
/// virtual destructors sit right after the interface methods, where Release
/// jumps on the last reference. A copy cut short sends those calls into the
/// heap. How long a vtable is cannot be read off it; entries copied from
/// past a shorter one are never called.
const D3D_VTABLE: usize = 64;

const DEV_CREATE_TEXTURE: usize = 23;
pub(crate) const DEV_CREATE_PIXEL_SHADER: usize = 106;
const DEV_VTABLE: usize = 256;

pub(crate) fn dev_orig(i: usize) -> usize {
    DEVICE.orig(i)
}

unsafe fn hook_d3d(d3d: Ptr) {
    let hooks = [(D3D_CREATE_DEVICE, create_device as *const () as usize)];
    if unsafe { D3D.adopt(d3d, D3D_VTABLE, &hooks) } {
        log("IDirect3D9 hooked");
    } else {
        log("IDirect3D9 of another class, left alone");
    }
}

unsafe extern "system" fn create_device(
    this: Ptr,
    adapter: u32,
    ty: u32,
    hwnd: Ptr,
    flags: u32,
    params: Ptr,
    out: *mut Ptr,
) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, u32, u32, Ptr, u32, Ptr, *mut Ptr) -> HRESULT =
        unsafe { std::mem::transmute(D3D.orig(D3D_CREATE_DEVICE)) };
    let r = unsafe { f(this, adapter, ty, hwnd, flags, params, out) };
    log(&format!(
        "CreateDevice(adapter {}, type {}, flags {:#x}) -> {:#x}",
        adapter, ty, flags, r
    ));
    if r >= 0 && !out.is_null() && unsafe { !(*out).is_null() } {
        unsafe { hook_device(*out) };
    }
    r
}

unsafe fn hook_device(dev: Ptr) {
    let traced = if trace::init() { trace::hooks().to_vec() } else { Vec::new() };
    let hooks = [
        [(DEV_CREATE_TEXTURE, create_texture as *const () as usize)].as_slice(),
        &phi::hooks(),
        &traced,
    ]
    .concat();
    if !unsafe { DEVICE.adopt(dev, DEV_VTABLE, &hooks) } {
        log("IDirect3DDevice9 of another class, left alone");
        return;
    }
    phi::device(dev);
    if trace::enabled() {
        trace::device(dev);
    }
    settings::watch();
    // The client's fonts from its MPQs before their first glyph (fontfile).
    std::thread::spawn(fontfile::preload);
    v2::device(dev);
    draw::create(dev);
    log("IDirect3DDevice9 hooked");
}

unsafe extern "system" fn create_texture(
    this: Ptr,
    w: u32,
    h: u32,
    levels: u32,
    usage: u32,
    format: u32,
    pool: u32,
    out: *mut Ptr,
    shared: *mut Ptr,
) -> HRESULT {
    let f: unsafe extern "system" fn(
        Ptr,
        u32,
        u32,
        u32,
        u32,
        u32,
        u32,
        *mut Ptr,
        *mut Ptr,
    ) -> HRESULT = unsafe { std::mem::transmute(dev_orig(DEV_CREATE_TEXTURE)) };
    let r = unsafe { f(this, w, h, levels, usage, format, pool, out, shared) };
    if r >= 0 && !out.is_null() {
        let tex = unsafe { *out } as usize;
        let glyph = w == 256 && h == 256 && format == D3DFMT_A4R4G4B4;
        // A freed texture's address can come back as anything: what we
        // knew about the old one goes. (Its vtable pointer, the glyph cache
        // tag, is the new object's own already.)
        fonts::owners().lock().unwrap().remove(&tex);
        if glyph {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            texture::hook(tex as Ptr, id);
            log(&format!(
                "glyph cache img{} {:#x} (levels {}, usage {:#x}, pool {})",
                id, tex, levels, usage, pool
            ));
        } else {
            texture::forget(tex);
        }
    }
    r
}
