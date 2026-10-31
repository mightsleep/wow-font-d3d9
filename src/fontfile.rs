//! Font files for the encoder. Files are looked up by the CGxFont name ("Fonts\ARIALN.TTF",
//! "Interface\AddOns\..."), case insensitively, under the folder of
//! Wow.exe, then in $WOW_FONT_D3D9_FONTS, then in the client's MPQs
//! (mpq.rs), as the client itself would: a loose file wins.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ttf_parser::Face;

use crate::log;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn CreateFileMappingW(
        file: *mut c_void,
        security: *const c_void,
        protect: u32,
        size_high: u32,
        size_low: u32,
        name: *const u16,
    ) -> *mut c_void;
    fn MapViewOfFile(
        map: *mut c_void,
        access: u32,
        off_high: u32,
        off_low: u32,
        len: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
}

const GENERIC_READ: u32 = 0x8000_0000;
const FILE_SHARE_READ: u32 = 1;
const OPEN_EXISTING: u32 = 3;
const PAGE_READONLY: u32 = 2;
const FILE_MAP_READ: u32 = 4;
const INVALID_HANDLE: *mut c_void = usize::MAX as *mut c_void;

/// A font file's bytes. One on disk is mapped, not read: a CJK font runs
/// to 26 MB, of which the parser touches the tables and the glyphs drawn,
/// and only those pages come in (the rest is address space, not memory).
/// One from an MPQ is inflated, so held.
pub enum FontData {
    Owned(Vec<u8>),
    Mapped { base: *const u8, len: usize },
}

// The view is read-only and lives until the drop.
unsafe impl Send for FontData {}
unsafe impl Sync for FontData {}

impl std::ops::Deref for FontData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            FontData::Owned(v) => v,
            FontData::Mapped { base, len } => unsafe { std::slice::from_raw_parts(*base, *len) },
        }
    }
}

impl Drop for FontData {
    fn drop(&mut self) {
        if let FontData::Mapped { base, .. } = self {
            unsafe { UnmapViewOfFile(*base as *const c_void) };
        }
    }
}

impl FontData {
    /// `path` mapped read-only, or None (an empty file maps to nothing).
    pub(crate) fn map(path: &Path) -> Option<FontData> {
        use std::os::windows::ffi::OsStrExt;
        let len = std::fs::metadata(path).ok()?.len() as usize;
        if len == 0 {
            return None;
        }
        let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            let file = CreateFileW(
                name.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if file == INVALID_HANDLE {
                return None;
            }
            let map = CreateFileMappingW(
                file,
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            );
            CloseHandle(file);
            if map.is_null() {
                return None;
            }
            // The view holds the mapping, and the mapping the file.
            let base = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0) as *const u8;
            CloseHandle(map);
            (!base.is_null()).then_some(FontData::Mapped { base, len })
        }
    }
}

fn search_dirs() -> &'static Vec<PathBuf> {
    static D: OnceLock<Vec<PathBuf>> = OnceLock::new();
    D.get_or_init(|| {
        let game = PathBuf::from(crate::exe_dir());
        let mut dirs = vec![game.join("Fonts"), game];
        // After the game's own folder: the client may ship its own versions.
        if let Some(d) = std::env::var_os("WOW_FONT_D3D9_FONTS") {
            dirs.push(PathBuf::from(d));
        }
        log(&format!("font search dirs: {:?}", dirs));
        dirs
    })
}

/// `rel` (a client path, backslashes) under `base`, each component matched
/// case-insensitively as the Windows client would.
fn resolve_ci(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut p = base.to_path_buf();
    for part in rel.split(['\\', '/']).filter(|s| !s.is_empty()) {
        let want = part.to_ascii_lowercase();
        let hit = std::fs::read_dir(&p)
            .ok()?
            .flatten()
            .find(|e| e.file_name().to_string_lossy().to_ascii_lowercase() == want)?;
        p = hit.path();
    }
    p.is_file().then_some(p)
}

/// A CGxFont name as a file: the whole relative path under each search
/// directory first (addon fonts: Interface\AddOns\...), then the file name
/// alone, all case-insensitively.
fn find(name: &str) -> Option<PathBuf> {
    if let Some(p) = search_dirs().iter().find_map(|d| resolve_ci(d, name)) {
        return Some(p);
    }
    let file = name.rsplit(['\\', '/']).next()?.to_ascii_lowercase();
    for d in search_dirs() {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().to_ascii_lowercase() == file {
                return Some(e.path());
            }
        }
    }
    None
}

/// Font data by CGxFont name, loaded once; None if the file is not found.
pub fn load(name: &str) -> Option<Arc<FontData>> {
    type Loaded = Option<Arc<FontData>>;
    static CACHE: OnceLock<Mutex<HashMap<String, Loaded>>> = OnceLock::new();
    // By the name in any case: the client spells one font both ways
    // (Fonts\ARIALN.TTF, Fonts\ARIALN.ttf), and paths are case-blind here.
    // Kept under the name as asked too, so that the asking again (every
    // upload) finds it without making a lowercase copy.
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(name) {
        return v.clone();
    }
    let key = name.to_ascii_lowercase();
    // One lock for both: in edition 2024 the guard of an `if let`
    // scrutinee lives through the block, so locking again inside it
    // deadlocked the client (Fonts\ARIALN.ttf after Fonts\ARIALN.TTF).
    {
        let mut c = cache.lock().unwrap();
        if let Some(v) = c.get(&key).cloned() {
            c.insert(name.to_string(), v.clone());
            return v;
        }
    }
    // Read without the lock: the preload thread may be opening the MPQs
    // for another font meanwhile. Two reads of one font race harmlessly.
    let v = find(name)
        .and_then(|p| {
            let data =
                FontData::map(&p).or_else(|| Some(FontData::Owned(std::fs::read(&p).ok()?)))?;
            Face::parse(&data, 0).ok()?;
            log(&format!(
                "font {} -> {}{}",
                name,
                p.display(),
                if matches!(data, FontData::Mapped { .. }) {
                    " (mapped)"
                } else {
                    ""
                }
            ));
            Some(Arc::new(data))
        })
        .or_else(|| from_mpq(name));
    if v.is_none() {
        log(&format!(
            "font {}: no file found, its cells stay bitmaps",
            name
        ));
    }
    let mut c = cache.lock().unwrap();
    c.insert(name.to_string(), v.clone());
    c.insert(key, v.clone());
    v
}

/// Data next to Wow.exe and the locale: its first four-letter subfolder
/// that holds a locale MPQ.
fn data_and_locale() -> Option<&'static (PathBuf, String)> {
    static D: OnceLock<Option<(PathBuf, String)>> = OnceLock::new();
    D.get_or_init(|| {
        let data = search_dirs()
            .iter()
            .find(|d| d.file_name().is_none_or(|n| n != "Fonts"))?
            .join("Data");
        let locale = std::fs::read_dir(&data).ok()?.flatten().find_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let is_locale = n.len() == 4
                && std::fs::read_dir(e.path()).ok()?.flatten().any(|f| {
                    f.file_name()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .starts_with("locale-")
                });
            is_locale.then_some(n)
        })?;
        Some((data, locale))
    })
    .as_ref()
}

/// The client's MPQ chain, open only while fonts are being read from it:
/// its tables (hash slots and blocks of 29 archives) take about 10 MB of a
/// 32-bit process, to read a handful of fonts. Those come in bursts (the
/// login, a loading screen, an addon's first frame), so the chain opens at
/// the first read of one and closes IDLE after the last (tidy): one open
/// per burst, nothing held in between.
static CHAIN: Mutex<Option<(crate::mpq::Chain, Instant)>> = Mutex::new(None);
const IDLE: Duration = Duration::from_secs(30);

fn read_mpq(name: &str) -> Option<Result<Vec<u8>, String>> {
    let mut c = CHAIN.lock().unwrap();
    if c.is_none() {
        let (data, locale) = data_and_locale()?;
        let t = Instant::now();
        let (chain, failed) = crate::mpq::Chain::open(data, locale);
        log(&format!(
            "mpq: {} archives ({}) opened in {:.0} ms{}",
            chain.archives.len(),
            locale,
            t.elapsed().as_secs_f64() * 1e3,
            if failed.is_empty() {
                String::new()
            } else {
                format!("; failed: {:?}", failed)
            }
        ));
        *c = Some((chain, Instant::now()));
    }
    let (chain, used) = c.as_mut()?;
    *used = Instant::now();
    chain.read(name)
}

/// The client's own fonts (3.3.5a, Fonts\ in its MPQs), about 370 kB
/// together.
const CLIENT_FONTS: [&str; 4] = [
    "Fonts\\FRIZQT__.TTF",
    "Fonts\\ARIALN.TTF",
    "Fonts\\MORPHEUS.TTF",
    "Fonts\\SKURRI.TTF",
];

/// Load the client's fonts ahead of their first glyph, on a thread of its
/// own (lib's hook_device starts it), and close the MPQ chain right after:
/// opening it takes ~126 ms, which the first font from it otherwise cost
/// inside an UnlockRect, and its ~10 MB of tables go again at once. A
/// font on disk (an HD patch's FRIZQT) is taken from there as ever.
pub fn preload() {
    let t = Instant::now();
    let got = CLIENT_FONTS.iter().filter(|n| load(n).is_some()).count();
    let closed = CHAIN.lock().unwrap().take().is_some();
    log(&format!(
        "fonts: {} of {} client fonts loaded ahead in {:.0} ms{}",
        got,
        CLIENT_FONTS.len(),
        t.elapsed().as_secs_f64() * 1e3,
        if closed { ", mpq: closed" } else { "" }
    ));
}

/// Close the MPQ chain if no font was read from it for IDLE. Call now and
/// then (settings::poll does).
pub fn tidy() {
    let mut c = CHAIN.lock().unwrap();
    if c.as_ref().is_some_and(|(_, used)| used.elapsed() >= IDLE) {
        *c = None;
        log(&format!("mpq: closed, {} s without a read", IDLE.as_secs()));
    }
}

fn from_mpq(name: &str) -> Option<Arc<FontData>> {
    match read_mpq(name)? {
        Ok(data) => {
            Face::parse(&data, 0).ok()?;
            log(&format!("font {} -> MPQ ({} bytes)", name, data.len()));
            Some(Arc::new(FontData::Owned(data)))
        }
        Err(e) => {
            log(&format!("font {}: in an MPQ but unreadable: {}", name, e));
            None
        }
    }
}
