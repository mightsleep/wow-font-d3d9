//! The d3d9.dll below this one, and the care that it is not this one.
//!
//! Which: $WOW_FONT_D3D9_REAL; else d3d9_real.dll next to Wow.exe, if
//! there is one (a DXVK or ReShade renamed, so that no .bat has to set a
//! variable); else the system directory's.
//!
//! A proxy that loads itself calls its own Direct3DCreate9 until the stack
//! ends, without a word. Three ways there, each caught by what the loader
//! and the linker already keep, no file read and no path compared:
//!
//! - The file is a copy of this DLL under another name. Every build of it
//!   carries MARK as the image's major version, a field of the PE header
//!   the linker fills in and nothing reads (build.rs). The file is first
//!   mapped as an image and no more, so none of its code runs; a module
//!   handle is the address the image is mapped at, and the field is two
//!   loads from there.
//! - The file is another wrapper whose Direct3DCreate9 is forwarded to
//!   "d3d9.dll", which the loader resolves to the module of that name
//!   already loaded: this one. The address it gives lies in this image,
//!   one unsigned compare against the image's base and size.
//! - The wrapper asks for "d3d9.dll" by name when it runs, gets this
//!   module and calls it. That is this module's export entered twice on
//!   one thread; the second entry is handed the system's.
//!
//! A file that is this proxy is not loaded and the system's is, with a
//! line in the log. A file that is missing or does not load is not
//! replaced by anything: the game then says it has no Direct3D, which is
//! better than a renderer nobody asked for.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

use crate::{GetProcAddress, LoadLibraryA, Ptr, log};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetSystemDirectoryA(buf: *mut u8, size: u32) -> u32;
    fn GetFileAttributesA(name: *const u8) -> u32;
    fn LoadLibraryExA(name: *const u8, file: Ptr, flags: u32) -> Ptr;
    fn FreeLibrary(module: Ptr) -> i32;
}

/// The image's major version of every build of this DLL: "D3D9" as far as
/// sixteen bits spell it. build.rs gives it to the linker; the flake's
/// check reads it back.
pub(crate) const MARK: u16 = 0xD3D9;

/// This module's handle, from DllMain.
static SELF: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn attached(module: Ptr) {
    SELF.store(module as usize, Relaxed);
}

/// What the PE header says of a loaded image.
#[derive(Clone, Copy)]
struct Image {
    base: usize,
    size: usize,
    major: u16,
    minor: u16,
}

impl Image {
    /// Of a module handle: the image's base (the two low bits flag a file
    /// mapped as data, which LoadLibraryA does not make). DOS header, its
    /// e_lfanew at 0x3c, "PE\0\0", the file header's 20 bytes, then the
    /// optional header: the image version at 44, the image's size at 56.
    unsafe fn of(module: Ptr) -> Option<Image> {
        let base = module as usize & !3;
        if base == 0 {
            return None;
        }
        unsafe {
            let u16_at = |at: usize| std::ptr::read_unaligned((base + at) as *const u16);
            let u32_at = |at: usize| std::ptr::read_unaligned((base + at) as *const u32);
            if u16_at(0) != 0x5A4D {
                return None;
            }
            let nt = u32_at(0x3c) as usize;
            if u32_at(nt) != 0x0000_4550 {
                return None;
            }
            let opt = nt + 24;
            Some(Image {
                base,
                size: u32_at(opt + 56) as usize,
                major: u16_at(opt + 44),
                minor: u16_at(opt + 46),
            })
        }
    }

    /// Whether the address is in the image: below the base it wraps to a
    /// number past any size.
    fn holds(&self, addr: usize) -> bool {
        addr.wrapping_sub(self.base) < self.size
    }

    /// The image as mapped. All of it is there to read: sections the file
    /// leaves out are zero pages.
    unsafe fn bytes(&self) -> &'static [u8] {
        unsafe { std::slice::from_raw_parts(self.base as *const u8, self.size) }
    }
}

/// build.rs packs the crate's version into the minor image version.
fn version_of(minor: u16) -> String {
    format!("{}.{}.{}", minor >> 12, (minor >> 4) & 0xff, minor & 0xf)
}

/// Why a loaded module must not be the one below: it is this proxy.
unsafe fn this_proxy(module: Ptr) -> Option<String> {
    let me = SELF.load(Relaxed);
    if module as usize == me {
        return Some("it is this very module".into());
    }
    let img = unsafe { Image::of(module) }?;
    if img.major == MARK {
        return Some(format!(
            "it is a copy of this proxy, {} (image version {:#06X})",
            version_of(img.minor),
            MARK
        ));
    }
    let f = unsafe { GetProcAddress(module, c"Direct3DCreate9".as_ptr() as *const u8) } as usize;
    let mine = unsafe { Image::of(me as Ptr) }?;
    (f != 0 && mine.holds(f)).then(|| {
        format!(
            "its Direct3DCreate9 is at {:#x}, inside this proxy: forwarded to d3d9.dll by name",
            f
        )
    })
}

/// DXVK's version as its d3d9.dll holds it for its log: "DXVK: ", a NUL,
/// the version ("3.1.1", older builds "v2.7.1").
fn dxvk_version(img: &Image) -> Option<String> {
    const NEEDLE: &[u8] = b"DXVK: \0";
    let b = unsafe { img.bytes() };
    let at = b.windows(NEEDLE.len()).position(|w| w == NEEDLE)? + NEEDLE.len();
    let v: String = b[at..]
        .iter()
        .take(24)
        .take_while(|&&c| c.is_ascii_graphic())
        .map(|&c| c as char)
        .collect();
    let v = v.trim_start_matches('v').to_string();
    v.starts_with(|c: char| c.is_ascii_digit()).then_some(v)
}

/// Whose Direct3D 9 the module is. Its path does not tell: DXVK is
/// installed over the system's file as often as beside the game. DXVK
/// exports DXVK_RegisterAnnotation, by number only (28257, in its
/// d3d9.def), which Wine's d3d9 has not; and Wine's ntdll says its
/// version.
fn whose(d3d9: Ptr) -> String {
    // A name below 0x10000 is an ordinal to GetProcAddress.
    const DXVK_REGISTER_ANNOTATION: usize = 28257;
    let dxvk = unsafe { !GetProcAddress(d3d9, DXVK_REGISTER_ANNOTATION as *const u8).is_null() };
    let dxvk = dxvk.then(|| {
        unsafe { Image::of(d3d9) }
            .and_then(|i| dxvk_version(&i))
            .map_or("DXVK".to_string(), |v| format!("DXVK {v}"))
    });
    match (dxvk, crate::wine_version()) {
        (Some(d), Some(w)) => format!("{d} under Wine {w}"),
        (Some(d), None) => d,
        (None, Some(w)) => format!("not DXVK, under Wine {w}: wined3d, or another wrapper"),
        (None, None) => "not DXVK: Windows' own, or another wrapper".into(),
    }
}

fn system_path() -> String {
    let mut buf = [0u8; 260];
    let n = unsafe { GetSystemDirectoryA(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    format!(
        "{}\\d3d9.dll",
        String::from_utf8_lossy(&buf[..n.min(buf.len())])
    )
}

fn exists(path: &str) -> bool {
    let c = std::ffi::CString::new(path).unwrap_or_default();
    unsafe { GetFileAttributesA(c.as_ptr() as *const u8) != u32::MAX }
}

/// A file that loaded and turned out to be this proxy.
struct Itself;

/// The file loaded and looked at: Ok(null) if it does not load. `why`
/// says how the path was come by.
fn load(path: &str, why: &str) -> Result<Ptr, Itself> {
    let c = std::ffi::CString::new(path).unwrap_or_default();
    // A look before anything of the file runs: mapped as an image and no
    // more (no imports bound, no DllMain), its header is there to read. A
    // copy of this proxy would otherwise attach, pin itself and write its
    // own lines into this log before it could be told.
    const AS_IMAGE: u32 = 0x20 | 0x02; // LOAD_LIBRARY_AS_IMAGE_RESOURCE | AS_DATAFILE
    let peek = unsafe { LoadLibraryExA(c.as_ptr() as *const u8, std::ptr::null_mut(), AS_IMAGE) };
    if !peek.is_null() {
        let img = unsafe { Image::of(peek) };
        unsafe { FreeLibrary(peek) };
        if let Some(img) = img.filter(|i| i.major == MARK) {
            log(&format!(
                "d3d9 below ({why}): {path} not loaded: it is a copy of this proxy, {} (image version {:#06X})",
                version_of(img.minor),
                MARK
            ));
            return Err(Itself);
        }
    }
    let h = unsafe { LoadLibraryA(c.as_ptr() as *const u8) };
    if h.is_null() {
        log(&format!("d3d9 below ({why}): {path} did not load"));
        return Ok(h);
    }
    // The path as asked for and the file the loader took for it: a bare
    // name is searched for.
    let file = crate::module_path(h);
    if let Some(reason) = unsafe { this_proxy(h) } {
        log(&format!(
            "d3d9 below ({why}): {path} ({file}) not used: {reason}"
        ));
        unsafe { FreeLibrary(h) };
        return Err(Itself);
    }
    log(&format!(
        "real d3d9 ({why}): {path} -> {h:?} ({file}), {}; glyph cache hooks {}",
        whose(h),
        if crate::enabled() { "on" } else { "off" }
    ));
    Ok(h)
}

/// The system directory's d3d9.dll, loaded once: what a file that turned
/// out to be this proxy is replaced by, and what a wrapper below gets
/// when it calls back in.
pub(crate) fn system() -> Ptr {
    static S: OnceLock<usize> = OnceLock::new();
    *S.get_or_init(|| load(&system_path(), "the system's").unwrap_or(std::ptr::null_mut()) as usize)
        as Ptr
}

/// The d3d9.dll below, loaded once.
pub(crate) fn real() -> Ptr {
    static R: OnceLock<usize> = OnceLock::new();
    *R.get_or_init(|| {
        let beside = crate::beside_exe("d3d9_real.dll");
        let (path, why) = match std::env::var("WOW_FONT_D3D9_REAL") {
            Ok(p) if !p.is_empty() => (p, "WOW_FONT_D3D9_REAL"),
            _ if exists(&beside) => (beside, "d3d9_real.dll next to Wow.exe"),
            _ => return system() as usize,
        };
        // A file named and not there stays an error; only one that is this
        // proxy is replaced.
        match load(&path, why) {
            Ok(h) => h as usize,
            Err(Itself) => {
                log("d3d9 below: the system's instead");
                system() as usize
            }
        }
    }) as Ptr
}

/// The thread inside one of this module's Direct3DCreate9 exports, 0 for
/// none.
static INSIDE: AtomicU32 = AtomicU32::new(0);

/// An export entered: `again` if this thread is in one already, which
/// only a module below calling back in can do.
pub(crate) struct Entered {
    first: bool,
    pub(crate) again: bool,
}

pub(crate) fn enter() -> Entered {
    let me = unsafe { GetCurrentThreadId() };
    match INSIDE.compare_exchange(0, me, Relaxed, Relaxed) {
        Ok(_) => Entered {
            first: true,
            again: false,
        },
        // Another thread's call is its own; nothing to tell from it.
        Err(owner) => {
            let again = owner == me;
            if again {
                static SAID: AtomicU32 = AtomicU32::new(0);
                if SAID.swap(1, Relaxed) == 0 {
                    log(
                        "Direct3DCreate9 called from below: the d3d9 under this proxy looked \
                         d3d9.dll up by name and found this proxy. It gets the system's.",
                    );
                }
            }
            Entered {
                first: false,
                again,
            }
        }
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        if self.first {
            INSIDE.store(0, Relaxed);
        }
    }
}
