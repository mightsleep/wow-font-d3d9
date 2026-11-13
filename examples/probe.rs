//! Does Windows take the DLL and its decoder: a 32-bit exe that loads the
//! proxy d3d9.dll next to it, makes a device of each kind the machine has
//! and gives it three pixel shaders. No game, no GPU asked for; meant for
//! a CI runner with Windows' own Direct3D 9 (and `nix run .#probe` under
//! Wine, where it shows only that the probe runs).
//!
//! The shaders: the decoder as the DLL embeds it, which has to be taken;
//! vkd3d's bytecode before build.rs fixes it, which reads two constants
//! in one instruction; and two lines by hand that use r32, one temporary
//! too many. Windows' runtime refuses the last two (docs/design.md, 20),
//! DXVK and wined3d take them. A device that refuses both checks what
//! went wrong on Windows before, so its yes to the decoder means
//! something; that is "validator: live".
//!
//! The proxy makes its own decoder when the device is made, and says so
//! in wow-font-d3d9.log, which is printed at the end.
//!
//!   probe.exe [--strict]
//!
//! --strict makes it a test: exit 1 unless a device with ps_3_0 has a
//! live validator and takes the decoder, from here and from the proxy.

#![allow(non_snake_case)]

use std::ffi::c_void;

type Ptr = *mut c_void;
type HRESULT = i32;

static DECODER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/decoder.ps3"));
static VKD3D: &[u8] = include_bytes!("../shaders/decoder.ps3");
static SMALL: &[u8] = include_bytes!("../shaders/bench_uv.ps3");

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryA(name: *const u8) -> Ptr;
    fn GetProcAddress(module: Ptr, name: *const u8) -> Ptr;
    fn GetLastError() -> u32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowExA(
        ex: u32,
        class: *const u8,
        name: *const u8,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: Ptr,
        menu: Ptr,
        instance: Ptr,
        param: Ptr,
    ) -> Ptr;
    fn GetDesktopWindow() -> Ptr;
}

const UNK_RELEASE: usize = 2;
const D3D_GET_ADAPTER_IDENTIFIER: usize = 5;
const D3D_CREATE_DEVICE: usize = 16;
const DEV_GET_DEVICE_CAPS: usize = 7;
const DEV_CREATE_PIXEL_SHADER: usize = 106;

const PS_3_0: u32 = 0xFFFF_0300;

#[repr(C)]
struct PresentParams {
    width: u32,
    height: u32,
    format: u32,
    count: u32,
    multisample: u32,
    quality: u32,
    swap_effect: u32,
    window: Ptr,
    windowed: i32,
    auto_depth: i32,
    depth_format: u32,
    flags: u32,
    refresh: u32,
    interval: u32,
}

/// The method at `index` of a COM object's vtable.
unsafe fn method(obj: Ptr, index: usize) -> *const c_void {
    unsafe { *(*(obj as *const *const *const c_void)).add(index) }
}

unsafe fn release(obj: Ptr) {
    if !obj.is_null() {
        unsafe {
            let f: unsafe extern "system" fn(Ptr) -> u32 =
                std::mem::transmute(method(obj, UNK_RELEASE));
            f(obj);
        }
    }
}

fn words(code: &[u8]) -> Vec<u32> {
    code.as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect()
}

/// ps_3_0 by hand: mov r32, c0; mov oC0, r32. The profile has r0 to r31.
fn r32() -> Vec<u32> {
    const MOV: u32 = 0x0200_0001;
    vec![
        PS_3_0,
        MOV,
        0x800F_0000 | 32,
        0xA0E4_0000,
        MOV,
        0x800F_0800,
        0x80E4_0000 | 32,
        0x0000_FFFF,
    ]
}

/// CreatePixelShader's answer to the bytecode.
fn takes(dev: Ptr, code: &[u32]) -> HRESULT {
    unsafe {
        let f: unsafe extern "system" fn(Ptr, *const u32, *mut Ptr) -> HRESULT =
            std::mem::transmute(method(dev, DEV_CREATE_PIXEL_SHADER));
        let mut ps: Ptr = std::ptr::null_mut();
        let r = f(dev, code.as_ptr(), &mut ps);
        if r >= 0 {
            release(ps);
        }
        r
    }
}

/// The adapter's description and driver, as D3DADAPTER_IDENTIFIER9 has
/// them: the driver's file and the description, 512 bytes each, the
/// version at 1056, the vendor and device ids after it.
fn adapter(d3d: Ptr) -> String {
    let mut id = [0u64; 138];
    let r = unsafe {
        let f: unsafe extern "system" fn(Ptr, u32, u32, *mut u64) -> HRESULT =
            std::mem::transmute(method(d3d, D3D_GET_ADAPTER_IDENTIFIER));
        f(d3d, 0, 0, id.as_mut_ptr())
    };
    if r < 0 {
        return format!("GetAdapterIdentifier {:#x}", r);
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

/// What one device said.
struct Seen {
    ps30: bool,
    decoder: bool,
    live: bool,
}

fn device(d3d: Ptr, wnd: Ptr, name: &str, kind: u32, vertex: u32) -> Option<Seen> {
    let mut pp = PresentParams {
        width: 64,
        height: 64,
        format: 21, // A8R8G8B8
        count: 1,
        multisample: 0,
        quality: 0,
        swap_effect: 1, // discard
        window: wnd,
        windowed: 1,
        auto_depth: 0,
        depth_format: 0,
        flags: 0,
        refresh: 0,
        interval: 0x8000_0000, // immediate
    };
    let mut dev: Ptr = std::ptr::null_mut();
    let r = unsafe {
        let f: unsafe extern "system" fn(
            Ptr,
            u32,
            u32,
            Ptr,
            u32,
            *mut PresentParams,
            *mut Ptr,
        ) -> HRESULT = std::mem::transmute(method(d3d, D3D_CREATE_DEVICE));
        f(d3d, 0, kind, wnd, vertex, &mut pp, &mut dev)
    };
    if r < 0 || dev.is_null() {
        println!("{}: CreateDevice {:#x}", name, r);
        return None;
    }
    // D3DCAPS9 is 304 bytes; its fields are 4 bytes each.
    let mut caps = [0u32; 76];
    let r = unsafe {
        let f: unsafe extern "system" fn(Ptr, *mut u32) -> HRESULT =
            std::mem::transmute(method(dev, DEV_GET_DEVICE_CAPS));
        f(dev, caps.as_mut_ptr())
    };
    let (ps, vs, slots) = (caps[204 / 4], caps[196 / 4], caps[300 / 4]);
    println!(
        "{}: caps {:#x}, ps {:#x}, vs {:#x}, ps30 slots {}",
        name, r, ps, vs, slots
    );
    let small = takes(dev, &words(SMALL));
    let decoder = takes(dev, &words(DECODER));
    let vkd3d = takes(dev, &words(VKD3D));
    let spare = takes(dev, &r32());
    println!("  a small ps_3_0                    -> {:#x}", small);
    println!("  the decoder, as embedded          -> {:#x}", decoder);
    println!("  vkd3d's, two constants at once    -> {:#x}", vkd3d);
    println!("  by hand, r32                      -> {:#x}", spare);
    let live = small >= 0 && vkd3d < 0 && spare < 0;
    println!(
        "  validator: {}",
        if live {
            "live"
        } else if small < 0 {
            "takes no ps_3_0 at all"
        } else {
            "takes what Windows refused, tells nothing"
        }
    );
    unsafe { release(dev) };
    Some(Seen {
        ps30: ps >= PS_3_0 && small >= 0,
        decoder: decoder >= 0,
        live,
    })
}

fn main() {
    let strict = std::env::args().any(|a| a == "--strict");
    let dir = std::env::current_exe().expect("the exe's path");
    let dir = dir.parent().expect("the exe's folder");
    let log = dir.join("wow-font-d3d9.log");
    let _ = std::fs::remove_file(&log);

    let path = std::ffi::CString::new(dir.join("d3d9.dll").to_str().expect("path")).unwrap();
    let lib = unsafe { LoadLibraryA(path.as_ptr() as *const u8) };
    if lib.is_null() {
        println!("LoadLibrary {:?}: error {}", path, unsafe {
            GetLastError()
        });
        std::process::exit(1);
    }
    println!("{:?} loaded", path);
    let create = unsafe { GetProcAddress(lib, c"Direct3DCreate9".as_ptr() as *const u8) };
    assert!(!create.is_null(), "no Direct3DCreate9 in the DLL");
    let create: unsafe extern "system" fn(u32) -> Ptr = unsafe { std::mem::transmute(create) };
    let d3d = unsafe { create(32) };
    if d3d.is_null() {
        println!("Direct3DCreate9: null");
        show(&log);
        std::process::exit(1);
    }
    println!("adapter: {}", adapter(d3d));

    let mut wnd = unsafe {
        CreateWindowExA(
            0,
            c"STATIC".as_ptr() as *const u8,
            c"probe".as_ptr() as *const u8,
            0x8000_0000, // WS_POPUP
            0,
            0,
            64,
            64,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if wnd.is_null() {
        println!(
            "CreateWindowEx: error {}, the desktop's window instead",
            unsafe { GetLastError() }
        );
        wnd = unsafe { GetDesktopWindow() };
    }

    // D3DDEVTYPE and D3DCREATE_*_VERTEXPROCESSING. The reference device
    // comes with the SDK only; the null one draws nothing.
    let kinds = [
        ("hal, hardware vertices", 1, 0x40),
        ("hal, software vertices", 1, 0x20),
        ("ref", 2, 0x20),
        ("nullref", 4, 0x20),
    ];
    let seen: Vec<Seen> = kinds
        .iter()
        .filter_map(|&(name, kind, vertex)| device(d3d, wnd, name, kind, vertex))
        .collect();
    unsafe { release(d3d) };

    let ours = show(&log);
    let proxy_made = ours
        .lines()
        .filter(|l| l.contains("curve decoder (v2) -> "))
        .count();
    let proxy_failed = ours
        .lines()
        .filter(|l| l.contains("curve decoder (v2) -> 0x0 "))
        .count();
    let good = seen.iter().any(|s| s.ps30 && s.live && s.decoder);
    let refused = seen.iter().any(|s| s.ps30 && !s.decoder);
    println!(
        "devices {}, with a live validator that takes the decoder: {}; \
         the proxy's own decoder: {} made, {} refused",
        seen.len(),
        if good { "yes" } else { "no" },
        proxy_made - proxy_failed,
        proxy_failed
    );
    if strict && (!good || refused || proxy_made == 0 || proxy_failed != 0) {
        std::process::exit(1);
    }
}

/// The proxy's log to stdout; its text.
fn show(log: &std::path::Path) -> String {
    let text = std::fs::read(log)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    println!("--- {} ---", log.display());
    if text.is_empty() {
        println!("(none: is the d3d9.dll here the proxy?)");
    }
    print!("{}", text);
    println!("---");
    text
}
