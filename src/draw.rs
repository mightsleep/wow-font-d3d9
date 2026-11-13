//! The decoder phi puts in for draws from a glyph cache (shaders/decoder.hlsl,
//! vkd3d's bytecode). If the device refuses it (or WOW_FONT_D3D9_DIAG=1),
//! diagnose says why in the log, and the same source through Windows'
//! d3dcompiler_47 stands in if the device takes that.

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use crate::{DEV_CREATE_PIXEL_SHADER, HRESULT, Ptr, dev_orig, log};

// vkd3d's bytecode with one constant an instruction (build.rs, ports.rs).
static DECODER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/decoder.ps3"));
static DECODER_HLSL: &str = include_str!("../shaders/decoder.hlsl");
/// A small vkd3d ps_3_0 (the bench's probe): does the device take vkd3d's
/// bytecode at all.
static PROBE: &[u8] = include_bytes!("../shaders/bench_uv.ps3");
static DECODER_PS: AtomicUsize = AtomicUsize::new(0);

const DEV_GET_DIRECT3D: usize = 6;
const DEV_GET_DEVICE_CAPS: usize = 7;
const D3D_GET_ADAPTER_IDENTIFIER: usize = 5;

/// The curve decoder of v2, or 0 if the device would not take it.
pub fn decoder() -> usize {
    DECODER_PS.load(Relaxed)
}

pub fn ours(ps: usize) -> bool {
    ps != 0 && ps == DECODER_PS.load(Relaxed)
}

/// CreatePixelShader on bytecode, its result as it is.
fn try_make(dev: Ptr, words: &[u32]) -> (usize, HRESULT) {
    let f: unsafe extern "system" fn(Ptr, *const u32, *mut Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_CREATE_PIXEL_SHADER)) };
    let mut ps: Ptr = std::ptr::null_mut();
    let r = unsafe { f(dev, words.as_ptr(), &mut ps) };
    (if r >= 0 { ps as usize } else { 0 }, r)
}

fn words(code: &[u8]) -> Vec<u32> {
    code.as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect()
}

fn make(dev: Ptr, code: &[u8], name: &str) -> usize {
    let (ps, r) = try_make(dev, &words(code));
    log(&format!("{} -> {:#x} ({:#x})", name, ps, r));
    ps
}

/// IUnknown::Release.
unsafe fn release(obj: usize) {
    if obj != 0 {
        unsafe {
            let vt = *(obj as *const *const usize);
            let f: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vt.add(2));
            f(obj);
        }
    }
}

/// A ps_3_0 of `n` instruction slots, assembled by hand: mov r0, c0; then
/// add r0, r0, c0 until n - 1; mov oC0, r0.
fn sized(n: usize) -> Vec<u32> {
    const R0_DST: u32 = 0x800F_0000;
    const R0_SRC: u32 = 0x80E4_0000;
    const C0_SRC: u32 = 0xA0E4_0000;
    const OC0_DST: u32 = 0x800F_0800;
    let mut v = vec![0xFFFF_0300, 0x0200_0001, R0_DST, C0_SRC];
    for _ in 0..n.saturating_sub(2) {
        v.extend([0x0300_0002, R0_DST, R0_SRC, C0_SRC]);
    }
    v.extend([0x0200_0001, OC0_DST, R0_SRC, 0x0000_FFFF]);
    v
}

/// Why the device would not take the decoder, to the log; the decoder by
/// d3dcompiler_47 if the device takes that one, else 0.
fn diagnose(dev: Ptr) -> usize {
    // D3DCAPS9 is 304 bytes; its fields are 4 bytes each.
    let mut caps = [0u32; 76];
    let f: unsafe extern "system" fn(Ptr, *mut u32) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_GET_DEVICE_CAPS)) };
    let r = unsafe { f(dev, caps.as_mut_ptr()) };
    let v = |at: usize| caps[at / 4];
    log(&format!(
        "diag: caps {:#x}: ps {:#x}, vs {:#x}, ps30 slots {}, vs30 slots {}, ps20 slots {}, \
         ps20 temps {}, ps20 dynamic flow {}, ps20 static flow {}, ps20 caps {:#x}, \
         ps instructions executed {}",
        r,
        v(204),
        v(196),
        v(300),
        v(296),
        v(280),
        v(272),
        v(268),
        v(276),
        v(264),
        v(292)
    ));
    // The adapter: D3DADAPTER_IDENTIFIER9, driver and description first,
    // the driver version (8 bytes) at 1056, vendor and device ids after.
    let mut d3d: Ptr = std::ptr::null_mut();
    let g: unsafe extern "system" fn(Ptr, *mut Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_GET_DIRECT3D)) };
    if unsafe { g(dev, &mut d3d) } >= 0 && !d3d.is_null() {
        let mut id = [0u64; 138];
        unsafe {
            let vt = *(d3d as *const *const usize);
            let ident: unsafe extern "system" fn(Ptr, u32, u32, *mut u64) -> HRESULT =
                std::mem::transmute(*vt.add(D3D_GET_ADAPTER_IDENTIFIER));
            ident(d3d, v(4), 0, id.as_mut_ptr());
            release(d3d as usize);
        }
        let b: &[u8] = unsafe { std::slice::from_raw_parts(id.as_ptr() as *const u8, 1104) };
        let text = |s: &[u8]| {
            String::from_utf8_lossy(s.split(|&c| c == 0).next().unwrap_or(&[])).into_owned()
        };
        let ver = u64::from_le_bytes(b[1056..1064].try_into().unwrap());
        let u = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        log(&format!(
            "diag: adapter {} ({}), driver {}.{}.{}.{}, vendor {:#06x} device {:#06x}",
            text(&b[512..1024]),
            text(&b[..512]),
            ver >> 48,
            (ver >> 32) & 0xffff,
            (ver >> 16) & 0xffff,
            ver & 0xffff,
            u(1064),
            u(1068)
        ));
    }
    // What the device takes: a small vkd3d shader, and shaders of known
    // size.
    let (ps, r) = try_make(dev, &words(PROBE));
    log(&format!("diag: vkd3d probe (10 instructions) -> {:#x}", r));
    unsafe { release(ps) };
    let mut sizes = String::new();
    for n in [256, 512, 513, 1024, 1400, 2048, 4096, 8192, 32768] {
        let (ps, r) = try_make(dev, &sized(n));
        sizes += &format!(" {}: {:#x};", n, r);
        unsafe { release(ps) };
    }
    log(&format!("diag: hand-made ps_3_0 by slots:{}", sizes));
    compiled(dev)
}

/// The decoder's source through d3dcompiler_47's D3DCompile, its errors
/// to the log and its bytecode next to Wow.exe; the shader if the device
/// takes it.
fn compiled(dev: Ptr) -> usize {
    let m = unsafe { crate::LoadLibraryA(c"d3dcompiler_47.dll".as_ptr() as *const u8) };
    if m.is_null() {
        log("diag: d3dcompiler_47.dll not found");
        return 0;
    }
    let p = unsafe { crate::GetProcAddress(m, c"D3DCompile".as_ptr() as *const u8) };
    if p.is_null() {
        log("diag: D3DCompile not found");
        return 0;
    }
    type Compile = unsafe extern "system" fn(
        *const u8,
        usize,
        *const u8,
        Ptr,
        Ptr,
        *const u8,
        *const u8,
        u32,
        u32,
        *mut Ptr,
        *mut Ptr,
    ) -> HRESULT;
    let compile: Compile = unsafe { std::mem::transmute(p) };
    const OPTIMIZATION_LEVEL3: u32 = 1 << 15;
    let (mut code, mut errors): (Ptr, Ptr) = (std::ptr::null_mut(), std::ptr::null_mut());
    let r = unsafe {
        compile(
            DECODER_HLSL.as_ptr(),
            DECODER_HLSL.len(),
            c"decoder.hlsl".as_ptr() as *const u8,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            c"main".as_ptr() as *const u8,
            c"ps_3_0".as_ptr() as *const u8,
            OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            &mut errors,
        )
    };
    // ID3DBlob: GetBufferPointer, GetBufferSize.
    let blob = |b: Ptr| -> Vec<u8> {
        unsafe {
            let vt = *(b as *const *const usize);
            let ptr: unsafe extern "system" fn(Ptr) -> *const u8 = std::mem::transmute(*vt.add(3));
            let size: unsafe extern "system" fn(Ptr) -> usize = std::mem::transmute(*vt.add(4));
            std::slice::from_raw_parts(ptr(b), size(b)).to_vec()
        }
    };
    if !errors.is_null() {
        let e = blob(errors);
        log(&format!(
            "diag: d3dcompiler_47: {}",
            String::from_utf8_lossy(&e).trim_end_matches('\0').trim()
        ));
        unsafe { release(errors as usize) };
    }
    if r < 0 || code.is_null() {
        log(&format!("diag: d3dcompiler_47 -> {:#x}", r));
        return 0;
    }
    let bytes = blob(code);
    unsafe { release(code as usize) };
    let path = crate::beside_exe("wow-font-d3d9-fxc.ps3");
    let saved = std::fs::write(&path, &bytes).is_ok();
    let (ps, r) = try_make(dev, &words(&bytes));
    log(&format!(
        "diag: d3dcompiler_47's decoder ({} bytes{}) -> {:#x} ({:#x})",
        bytes.len(),
        if saved {
            ", saved as wow-font-d3d9-fxc.ps3"
        } else {
            ""
        },
        ps,
        r
    ));
    ps
}

pub fn create(dev: Ptr) {
    let mut ps = make(dev, DECODER, "curve decoder (v2)");
    if ps == 0 || std::env::var("WOW_FONT_D3D9_DIAG").is_ok_and(|v| v == "1") {
        let fxc = diagnose(dev);
        if ps == 0 {
            ps = fxc;
        } else {
            unsafe { release(fxc) };
        }
    }
    DECODER_PS.store(ps, Relaxed);
}
