//! Proxy d3d9.dll. Sits next to Wow.exe, loads the real d3d9.dll
//! ($WOW_FONT_D3D9_REAL, else the one in the system directory: Windows'
//! own, or DXVK or wined3d under Wine) and forwards everything. Unless
//! WOW_FONT_D3D9=0 it writes vector glyphs into the client's glyph caches
//! (256x256 A4R4G4B4 textures; texture, payload) and has them drawn by the
//! decoder (phi, draw).
//!
//! Hooks point an object at a patched copy of its class's vtable (Class);
//! no code anywhere is patched. Log: wow-font-d3d9.log next to Wow.exe.
//! WOW_FONT_D3D9_CURSOR=1 adds the one hook outside Direct3D 9, a window
//! procedure (cursor).

#![allow(non_snake_case)]

#[allow(dead_code)]
mod atlas;
mod cache;
#[allow(dead_code)]
mod coverage;
mod cursor;
mod draw;
mod fontfile;
mod fonts;
#[allow(dead_code)]
mod glyph;
mod mem;
mod mpq;
mod payload;
mod phi;
mod record;
mod settings;
mod texture;
mod trace;
mod union;
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
    fn GetModuleHandleA(name: *const u8) -> Ptr;
}

const D3DFMT_A4R4G4B4: u32 = 26;
const D3DERR_NOTAVAILABLE: HRESULT = 0x8876086Au32 as i32;

/// The folder of Wow.exe (the launcher's working directory can be anything).
pub(crate) fn exe_dir() -> &'static str {
    static D: OnceLock<String> = OnceLock::new();
    D.get_or_init(|| {
        let mut buf = [0u8; 260];
        let n =
            unsafe { GetModuleFileNameA(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) }
                as usize;
        let exe = String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned();
        exe.rfind('\\')
            .map_or(String::new(), |i| exe[..i].to_string())
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

/// Today in UTC as year-month-day, by the days since 1970 (the civil
/// calendar from a day count, as in Hinnant's date algorithms).
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, m, d) = civil((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// (year, month, day) of the day that many days after 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Where the log starts again: 2 MiB, a few weeks of play or a day with
/// tracing on.
const LOG_MAX: u64 = 2 << 20;

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
        // Runs append, so the last ones are there to send. A log past
        // LOG_MAX starts again and the old one stays beside it as .old
        // (the one before that goes): two files of a size to attach,
        // however long the DLL has been in the folder.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX) {
            let old = format!("{}.old", path);
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
        Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok(),
        )
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
        // The path as asked for and the file the loader took for it: a bare
        // name in WOW_FONT_D3D9_REAL is searched for.
        log(&format!(
            "real d3d9: {} -> {:?}{}; glyph cache hooks {}",
            path,
            h,
            if h.is_null() {
                String::new()
            } else {
                format!(" ({}), {}", module_path(h), whose(h))
            },
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

// ---- what a log sent in has to say about the machine -------------------

/// The WOW_FONT_D3D9 variables the process was started with: a start from
/// a .bat and one from the exe differ in these and in nothing else here.
fn environment() -> String {
    let mut set: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("WOW_FONT_D3D9"))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    set.sort();
    if set.is_empty() {
        "none of the WOW_FONT_D3D9 variables set".into()
    } else {
        set.join(", ")
    }
}

/// Whose Direct3D 9 the real d3d9.dll is. Its path does not tell: DXVK is
/// installed over the system's file as often as beside the game. DXVK
/// exports DXVK_RegisterAnnotation, by number only (28257, in its
/// d3d9.def), which Wine's d3d9 has not; and Wine's ntdll says its
/// version.
fn whose(d3d9: Ptr) -> String {
    // A name below 0x10000 is an ordinal to GetProcAddress.
    const DXVK_REGISTER_ANNOTATION: usize = 28257;
    let dxvk = unsafe { !GetProcAddress(d3d9, DXVK_REGISTER_ANNOTATION as *const u8).is_null() };
    let wine = wine_version();
    match (dxvk, wine) {
        (true, Some(w)) => format!("DXVK under Wine {w}"),
        (true, None) => "DXVK".into(),
        (false, Some(w)) => format!("not DXVK, under Wine {w}: wined3d, or another wrapper"),
        (false, None) => "not DXVK: Windows' own, or another wrapper".into(),
    }
}

/// Wine's version, if this is Wine: ntdll's wine_get_version, which
/// Windows' ntdll does not have.
fn wine_version() -> Option<String> {
    let ntdll = unsafe { GetModuleHandleA(c"ntdll.dll".as_ptr() as *const u8) };
    if ntdll.is_null() {
        return None;
    }
    let f = unsafe { GetProcAddress(ntdll, c"wine_get_version".as_ptr() as *const u8) };
    if f.is_null() {
        return None;
    }
    let f: unsafe extern "C" fn() -> *const std::ffi::c_char = unsafe { std::mem::transmute(f) };
    let v = unsafe { f() };
    (!v.is_null()).then(|| {
        unsafe { std::ffi::CStr::from_ptr(v) }
            .to_string_lossy()
            .into_owned()
    })
}

/// A module's file, as the loader found it.
fn module_path(module: Ptr) -> String {
    let mut buf = [0u8; 260];
    let n = unsafe { GetModuleFileNameA(module, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned()
}

const D3D_GET_ADAPTER_IDENTIFIER: usize = 5;

/// The adapter's description and driver, as D3DADAPTER_IDENTIFIER9 has
/// them: the driver's file and the description, 512 bytes each, the
/// version at 1056, the vendor and device ids after it.
unsafe fn adapter(d3d: Ptr, adapter: u32) -> String {
    let mut id = [0u64; 138];
    let f: unsafe extern "system" fn(Ptr, u32, u32, *mut u64) -> HRESULT =
        unsafe { std::mem::transmute(D3D.orig(D3D_GET_ADAPTER_IDENTIFIER)) };
    let r = unsafe { f(d3d, adapter, 0, id.as_mut_ptr()) };
    if r < 0 {
        return format!("GetAdapterIdentifier {r:#x}");
    }
    let b: &[u8] = unsafe { std::slice::from_raw_parts(id.as_ptr() as *const u8, 1104) };
    let text =
        |s: &[u8]| String::from_utf8_lossy(s.split(|&c| c == 0).next().unwrap_or(&[])).into_owned();
    let ver = u64::from_le_bytes(b[1056..1064].try_into().unwrap());
    let u = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    format!(
        "{} ({}), driver {}.{}.{}.{}, vendor {:#06x} device {:#06x}",
        text(&b[512..1024]),
        text(&b[..512]),
        ver >> 48,
        (ver >> 32) & 0xffff,
        (ver >> 16) & 0xffff,
        ver & 0xffff,
        u(1064),
        u(1068)
    )
}

/// D3DPRESENT_PARAMETERS in words: fourteen fields of four bytes in a
/// 32-bit process. Read after the call, when the runtime has filled in
/// what the client left to it (a window's size, the format).
pub(crate) unsafe fn present_params(params: Ptr) -> String {
    if params.is_null() {
        return "no present parameters".into();
    }
    let p: [u32; 14] = unsafe { std::ptr::read_unaligned(params as *const [u32; 14]) };
    let format = |f: u32| match f {
        0 => "unknown".to_string(),
        21 => "A8R8G8B8".into(),
        22 => "X8R8G8B8".into(),
        23 => "R5G6B5".into(),
        35 => "A2R10G10B10".into(),
        75 => "D24S8".into(),
        77 => "D24X8".into(),
        80 => "D16".into(),
        n => format!("format {n}"),
    };
    format!(
        "{}x{} {}, {}, {} back buffer(s), multisample {} (quality {}), swap effect {}, depth {}, refresh {} Hz, interval {:#x}, flags {:#x}",
        p[0],
        p[1],
        format(p[2]),
        if p[8] != 0 { "windowed" } else { "fullscreen" },
        p[3],
        p[4],
        p[5],
        p[6],
        if p[9] != 0 {
            format(p[10])
        } else {
            "none".into()
        },
        p[12],
        p[13],
        p[11]
    )
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
            // The lines carry a time of day only: the date here tells one
            // run from another; the version and the commit tell which
            // DLL wrote them (a build between releases has the last
            // release's version).
            log(&format!(
                "proxy attached: {} ({}), {} UTC, pinned: {}",
                env!("CARGO_PKG_VERSION"),
                env!("WOW_FONT_D3D9_REV"),
                today(),
                pinned != 0
            ));
            log(&format!("environment: {}", environment()));
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
        #[allow(clippy::unused_unit)]
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
        Class {
            table: OnceLock::new(),
        }
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
        !obj.is_null()
            && self
                .table
                .get()
                .is_some_and(|t| unsafe { *(obj as *const usize) } == t.1)
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
    // Once a device: the client makes one more after a lost one.
    log(&format!("adapter: {}", unsafe {
        self::adapter(this, adapter)
    }));
    log(&format!("present: {}", unsafe { present_params(params) }));
    if r >= 0 && !out.is_null() && unsafe { !(*out).is_null() } {
        unsafe { hook_device(*out) };
        unsafe { cursor::device(hwnd, params) };
    }
    r
}

unsafe fn hook_device(dev: Ptr) {
    let traced = if trace::init() {
        trace::hooks().to_vec()
    } else {
        Vec::new()
    };
    let hooks = [
        [(DEV_CREATE_TEXTURE, create_texture as *const () as usize)].as_slice(),
        &phi::hooks(),
        &traced,
        &cursor::hooks(),
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
