//! The decoder without the game: glyph caches built from font files by the
//! proxy's own encoder, drawn through Direct3D 9 the way the client draws
//! text (d3dbc -> DXVK -> SPIR-V -> driver), and measured against an exact
//! reference. Run under Wine with DXVK's d3d9.dll next to it:
//! nix run .#bench.
//!
//! A page is one cache drawn at one scale. The exact reference: a probe
//! shader (shaders/bench_uv.hlsl) records where each pixel samples, and
//! there the CPU takes the glyph's area in the pixel's box (coverage.rs) and
//! the distance to the outline. Each shader is also checked against
//! the first.
//!
//!   bench.exe --out DIR --ps NAME=FILE.ps3 ... --font FILE:PX:OUTLINE ...
//!             [--scales 0.5,1,1.5,2] [--reps 10] [--rounds 7] [--no-time]
//!             [--pages 3,7] [--chars TEXT] [--color RRGGBB]
//!             [--max-gross 1:0.05,2:0.25]
//!
//! --max-gross SCALE:PCT,... makes it a test: exit 1 when a shader has
//! more than PCT % of its text texels off the reference by over 16/255
//! at that scale (`nix run .#gate`, in CI).

#![allow(non_snake_case)]

#[allow(dead_code)]
#[path = "../src/atlas.rs"]
mod atlas;
#[allow(dead_code)]
#[path = "../src/coverage.rs"]
mod coverage;
#[path = "../src/glyph.rs"]
mod glyph;
#[path = "../src/union.rs"]
mod union;

use std::ffi::c_void;
use std::fmt::Write as _;
use std::path::PathBuf;

type Ptr = *mut c_void;
type HRESULT = i32;

static VS: &[u8] = include_bytes!("../shaders/bench.vs3");
/// The probe: p and fw where each pixel samples (shaders/bench_uv.hlsl).
static UV_PS: &[u8] = include_bytes!("../shaders/bench_uv.ps3");

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryA(name: *const u8) -> Ptr;
    fn GetProcAddress(module: Ptr, name: *const u8) -> Ptr;
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
        inst: Ptr,
        param: Ptr,
    ) -> Ptr;
}

// ---- COM by vtable ------------------------------------------------------

/// Method `i` of a COM object, as a function pointer of type F.
unsafe fn m<F: Copy>(obj: Ptr, i: usize) -> F {
    unsafe {
        let vt = *(obj as *const *const usize);
        std::mem::transmute_copy(&*vt.add(i))
    }
}

fn ok(r: HRESULT, what: &str) {
    if r < 0 {
        panic!("{} failed: {:#x}", what, r as u32);
    }
}

const D3D_CREATE_DEVICE: usize = 16;
const DEV_CREATE_TEXTURE: usize = 23;
const DEV_GET_RENDER_TARGET_DATA: usize = 32;
const DEV_CREATE_RENDER_TARGET: usize = 28;
const DEV_CREATE_OFFSCREEN_PLAIN_SURFACE: usize = 36;
const DEV_SET_RENDER_TARGET: usize = 37;
const DEV_BEGIN_SCENE: usize = 41;
const DEV_END_SCENE: usize = 42;
const DEV_CLEAR: usize = 43;
const DEV_SET_RENDER_STATE: usize = 57;
const DEV_SET_TEXTURE: usize = 65;
const DEV_SET_SAMPLER_STATE: usize = 69;
const DEV_DRAW_PRIMITIVE_UP: usize = 83;
const DEV_CREATE_VERTEX_DECLARATION: usize = 86;
const DEV_SET_VERTEX_DECLARATION: usize = 87;
const DEV_CREATE_VERTEX_SHADER: usize = 91;
const DEV_SET_VERTEX_SHADER: usize = 92;
const DEV_CREATE_PIXEL_SHADER: usize = 106;
const DEV_SET_PIXEL_SHADER: usize = 107;
const DEV_SET_PS_CONSTANT_F: usize = 109;
const DEV_CREATE_QUERY: usize = 118;
const TEX_LOCK_RECT: usize = 19;
const TEX_UNLOCK_RECT: usize = 20;
const SURF_LOCK_RECT: usize = 13;
const SURF_UNLOCK_RECT: usize = 14;
const Q_ISSUE: usize = 6;
const Q_GET_DATA: usize = 7;

const FMT_A4R4G4B4: u32 = 26;
const FMT_A32B32G32R32F: u32 = 116;
const FMT_L8: u32 = 50;
const FMT_A8L8: u32 = 51;
const FMT_A16B16G16R16: u32 = 36;
const POOL_MANAGED: u32 = 1;
const POOL_SYSTEMMEM: u32 = 2;

/// The decoder's constant registers (src/phi.rs) and the settings' defaults.
const CFG: u32 = 200;
const GAMMA: f32 = 1.5;
const OUTLINE_EXTRA: f32 = 0.25;
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

#[repr(C)]
struct Locked {
    pitch: i32,
    bits: *mut u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    pos: [f32; 4],
    color: u32,
    uv: [f32; 2],
}

struct Device(Ptr);

impl Device {
    fn new() -> Device {
        unsafe {
            let lib = LoadLibraryA(c"d3d9.dll".as_ptr() as *const u8);
            assert!(!lib.is_null(), "no d3d9.dll");
            let create = GetProcAddress(lib, c"Direct3DCreate9".as_ptr() as *const u8);
            let create: unsafe extern "system" fn(u32) -> Ptr = std::mem::transmute(create);
            let d3d = create(32);
            assert!(!d3d.is_null(), "Direct3DCreate9");
            let wnd = CreateWindowExA(
                0,
                c"STATIC".as_ptr() as *const u8,
                c"bench".as_ptr() as *const u8,
                0x8000_0000, // WS_POPUP
                0,
                0,
                64,
                64,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert!(!wnd.is_null(), "CreateWindowEx");
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
            let f: unsafe extern "system" fn(
                Ptr,
                u32,
                u32,
                Ptr,
                u32,
                *mut PresentParams,
                *mut Ptr,
            ) -> HRESULT = m(d3d, D3D_CREATE_DEVICE);
            // Hardware vertex processing.
            ok(f(d3d, 0, 1, wnd, 0x40, &mut pp, &mut dev), "CreateDevice");
            Device(dev)
        }
    }

    fn create_texture(&self, words: &[u16]) -> Ptr {
        unsafe {
            let f: unsafe extern "system" fn(
                Ptr,
                u32,
                u32,
                u32,
                u32,
                u32,
                u32,
                *mut Ptr,
                Ptr,
            ) -> HRESULT = m(self.0, DEV_CREATE_TEXTURE);
            let mut tex: Ptr = std::ptr::null_mut();
            ok(
                f(
                    self.0,
                    256,
                    256,
                    1,
                    0,
                    FMT_A4R4G4B4,
                    POOL_MANAGED,
                    &mut tex,
                    std::ptr::null_mut(),
                ),
                "CreateTexture",
            );
            let lock: unsafe extern "system" fn(
                Ptr,
                u32,
                *mut Locked,
                *const c_void,
                u32,
            ) -> HRESULT = m(tex, TEX_LOCK_RECT);
            let unlock: unsafe extern "system" fn(Ptr, u32) -> HRESULT = m(tex, TEX_UNLOCK_RECT);
            let mut l = Locked {
                pitch: 0,
                bits: std::ptr::null_mut(),
            };
            ok(lock(tex, 0, &mut l, std::ptr::null(), 0), "LockRect");
            for y in 0..256 {
                let row = l.bits.offset(y as isize * l.pitch as isize) as *mut u16;
                std::ptr::copy_nonoverlapping(words[y * 256..].as_ptr(), row, 256);
            }
            ok(unlock(tex, 0), "UnlockRect");
            tex
        }
    }

    /// A 256 x 256 cover texture, managed, from bytes row by row: L8 at a
    /// byte a texel, A8L8 (L first) at two.
    fn create_texture_u8(&self, bytes: &[u8], per: usize) -> Ptr {
        unsafe {
            let f: unsafe extern "system" fn(
                Ptr,
                u32,
                u32,
                u32,
                u32,
                u32,
                u32,
                *mut Ptr,
                Ptr,
            ) -> HRESULT = m(self.0, DEV_CREATE_TEXTURE);
            let mut tex: Ptr = std::ptr::null_mut();
            let format = if per == 2 { FMT_A8L8 } else { FMT_L8 };
            ok(
                f(
                    self.0,
                    256,
                    256,
                    1,
                    0,
                    format,
                    POOL_MANAGED,
                    &mut tex,
                    std::ptr::null_mut(),
                ),
                "CreateTexture (cover)",
            );
            let lock: unsafe extern "system" fn(
                Ptr,
                u32,
                *mut Locked,
                *const c_void,
                u32,
            ) -> HRESULT = m(tex, TEX_LOCK_RECT);
            let unlock: unsafe extern "system" fn(Ptr, u32) -> HRESULT = m(tex, TEX_UNLOCK_RECT);
            let mut l = Locked {
                pitch: 0,
                bits: std::ptr::null_mut(),
            };
            ok(lock(tex, 0, &mut l, std::ptr::null(), 0), "LockRect (L8)");
            for y in 0..256 {
                let row = l.bits.offset(y as isize * l.pitch as isize);
                std::ptr::copy_nonoverlapping(bytes[y * 256 * per..].as_ptr(), row, 256 * per);
            }
            ok(unlock(tex, 0), "UnlockRect (L8)");
            tex
        }
    }

    /// The texel words (stage 4), A16B16G16R16, managed, row by row.
    fn create_texture_u16x4(&self, words: &[[u16; 4]]) -> Ptr {
        unsafe {
            let f: unsafe extern "system" fn(
                Ptr,
                u32,
                u32,
                u32,
                u32,
                u32,
                u32,
                *mut Ptr,
                Ptr,
            ) -> HRESULT = m(self.0, DEV_CREATE_TEXTURE);
            let mut tex: Ptr = std::ptr::null_mut();
            ok(
                f(
                    self.0,
                    256,
                    256,
                    1,
                    0,
                    FMT_A16B16G16R16,
                    POOL_MANAGED,
                    &mut tex,
                    std::ptr::null_mut(),
                ),
                "CreateTexture (words)",
            );
            let lock: unsafe extern "system" fn(
                Ptr,
                u32,
                *mut Locked,
                *const c_void,
                u32,
            ) -> HRESULT = m(tex, TEX_LOCK_RECT);
            let unlock: unsafe extern "system" fn(Ptr, u32) -> HRESULT = m(tex, TEX_UNLOCK_RECT);
            let mut l = Locked {
                pitch: 0,
                bits: std::ptr::null_mut(),
            };
            ok(
                lock(tex, 0, &mut l, std::ptr::null(), 0),
                "LockRect (words)",
            );
            for y in 0..256 {
                // D3D's A16B16G16R16 is R first in memory: x, y, z, w.
                let row = l.bits.offset(y as isize * l.pitch as isize) as *mut [u16; 4];
                std::ptr::copy_nonoverlapping(words[y * 256..].as_ptr(), row, 256);
            }
            ok(unlock(tex, 0), "UnlockRect (words)");
            tex
        }
    }

    /// A w x h A32B32G32R32F texture, managed, from texels row by row.
    fn create_texture_f32(&self, w: u32, h: u32, texels: &[[f32; 4]]) -> Ptr {
        unsafe {
            let f: unsafe extern "system" fn(
                Ptr,
                u32,
                u32,
                u32,
                u32,
                u32,
                u32,
                *mut Ptr,
                Ptr,
            ) -> HRESULT = m(self.0, DEV_CREATE_TEXTURE);
            let mut tex: Ptr = std::ptr::null_mut();
            ok(
                f(
                    self.0,
                    w,
                    h,
                    1,
                    0,
                    FMT_A32B32G32R32F,
                    POOL_MANAGED,
                    &mut tex,
                    std::ptr::null_mut(),
                ),
                "CreateTexture (f32)",
            );
            let lock: unsafe extern "system" fn(
                Ptr,
                u32,
                *mut Locked,
                *const c_void,
                u32,
            ) -> HRESULT = m(tex, TEX_LOCK_RECT);
            let unlock: unsafe extern "system" fn(Ptr, u32) -> HRESULT = m(tex, TEX_UNLOCK_RECT);
            let mut l = Locked {
                pitch: 0,
                bits: std::ptr::null_mut(),
            };
            ok(lock(tex, 0, &mut l, std::ptr::null(), 0), "LockRect (f32)");
            for y in 0..h as usize {
                let row = l.bits.offset(y as isize * l.pitch as isize) as *mut [f32; 4];
                std::ptr::copy_nonoverlapping(texels[y * w as usize..].as_ptr(), row, w as usize);
            }
            ok(unlock(tex, 0), "UnlockRect (f32)");
            tex
        }
    }

    fn render_target(&self, size: u32) -> (Ptr, Ptr) {
        unsafe {
            let f: unsafe extern "system" fn(
                Ptr,
                u32,
                u32,
                u32,
                u32,
                u32,
                i32,
                *mut Ptr,
                Ptr,
            ) -> HRESULT = m(self.0, DEV_CREATE_RENDER_TARGET);
            let mut rt: Ptr = std::ptr::null_mut();
            ok(
                f(
                    self.0,
                    size,
                    size,
                    FMT_A32B32G32R32F,
                    0,
                    0,
                    0,
                    &mut rt,
                    std::ptr::null_mut(),
                ),
                "CreateRenderTarget",
            );
            let f: unsafe extern "system" fn(Ptr, u32, u32, u32, u32, *mut Ptr, Ptr) -> HRESULT =
                m(self.0, DEV_CREATE_OFFSCREEN_PLAIN_SURFACE);
            let mut sys: Ptr = std::ptr::null_mut();
            ok(
                f(
                    self.0,
                    size,
                    size,
                    FMT_A32B32G32R32F,
                    POOL_SYSTEMMEM,
                    &mut sys,
                    std::ptr::null_mut(),
                ),
                "CreateOffscreenPlainSurface",
            );
            let f: unsafe extern "system" fn(Ptr, u32, Ptr) -> HRESULT =
                m(self.0, DEV_SET_RENDER_TARGET);
            ok(f(self.0, 0, rt), "SetRenderTarget");
            (rt, sys)
        }
    }

    /// The render target's texels, RGBA f32, `size` square.
    fn read(&self, rt: Ptr, sys: Ptr, size: u32) -> Vec<[f32; 4]> {
        unsafe {
            let f: unsafe extern "system" fn(Ptr, Ptr, Ptr) -> HRESULT =
                m(self.0, DEV_GET_RENDER_TARGET_DATA);
            ok(f(self.0, rt, sys), "GetRenderTargetData");
            let lock: unsafe extern "system" fn(Ptr, *mut Locked, *const c_void, u32) -> HRESULT =
                m(sys, SURF_LOCK_RECT);
            let unlock: unsafe extern "system" fn(Ptr) -> HRESULT = m(sys, SURF_UNLOCK_RECT);
            let mut l = Locked {
                pitch: 0,
                bits: std::ptr::null_mut(),
            };
            ok(lock(sys, &mut l, std::ptr::null(), 0x10), "LockRect (read)");
            let n = size as usize;
            let mut out = vec![[0f32; 4]; n * n];
            for y in 0..n {
                let row = l.bits.offset(y as isize * l.pitch as isize) as *const [f32; 4];
                std::ptr::copy_nonoverlapping(row, out[y * n..].as_mut_ptr(), n);
            }
            ok(unlock(sys), "UnlockRect (read)");
            out
        }
    }

    fn pixel_shader(&self, code: &[u8]) -> Ptr {
        let words: Vec<u32> = code
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        unsafe {
            let f: unsafe extern "system" fn(Ptr, *const u32, *mut Ptr) -> HRESULT =
                m(self.0, DEV_CREATE_PIXEL_SHADER);
            let mut ps: Ptr = std::ptr::null_mut();
            ok(f(self.0, words.as_ptr(), &mut ps), "CreatePixelShader");
            ps
        }
    }

    /// Everything that stays put: the vertex shader and its declaration,
    /// no blending, depth or culling, point sampling (the decoder reads
    /// texel centres, the client's bitmap path would filter).
    fn fixed_state(&self) {
        unsafe {
            let words: Vec<u32> = VS
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let f: unsafe extern "system" fn(Ptr, *const u32, *mut Ptr) -> HRESULT =
                m(self.0, DEV_CREATE_VERTEX_SHADER);
            let mut vs: Ptr = std::ptr::null_mut();
            ok(f(self.0, words.as_ptr(), &mut vs), "CreateVertexShader");
            let f: unsafe extern "system" fn(Ptr, Ptr) -> HRESULT =
                m(self.0, DEV_SET_VERTEX_SHADER);
            ok(f(self.0, vs), "SetVertexShader");
            // D3DVERTEXELEMENT9: stream, offset, type, method, usage, index.
            let decl: [[u16; 4]; 4] = [
                el(0, 0, 3, 0, 0),     // FLOAT4 POSITION
                el(0, 16, 4, 10, 0),   // D3DCOLOR COLOR0
                el(0, 20, 1, 5, 0),    // FLOAT2 TEXCOORD0
                el(0xFF, 0, 17, 0, 0), // D3DDECL_END
            ];
            let f: unsafe extern "system" fn(Ptr, *const c_void, *mut Ptr) -> HRESULT =
                m(self.0, DEV_CREATE_VERTEX_DECLARATION);
            let mut d: Ptr = std::ptr::null_mut();
            ok(
                f(self.0, decl.as_ptr() as *const c_void, &mut d),
                "CreateVertexDeclaration",
            );
            let f: unsafe extern "system" fn(Ptr, Ptr) -> HRESULT =
                m(self.0, DEV_SET_VERTEX_DECLARATION);
            ok(f(self.0, d), "SetVertexDeclaration");
            let rs: unsafe extern "system" fn(Ptr, u32, u32) -> HRESULT =
                m(self.0, DEV_SET_RENDER_STATE);
            for (state, value) in [(7, 0), (14, 0), (15, 0), (22, 1), (27, 0)] {
                rs(self.0, state, value);
            }
            let ss: unsafe extern "system" fn(Ptr, u32, u32, u32) -> HRESULT =
                m(self.0, DEV_SET_SAMPLER_STATE);
            // The glyph cache, then format v2's column map and atlas (float
            // textures: point sampling, which they need on older hardware).
            for stage in 0..5 {
                for (state, value) in [(1, 3), (2, 3), (5, 1), (6, 1), (7, 0)] {
                    ss(self.0, stage, state, value);
                }
            }
        }
    }

    fn set_ps(&self, ps: Ptr) {
        unsafe {
            let f: unsafe extern "system" fn(Ptr, Ptr) -> HRESULT = m(self.0, DEV_SET_PIXEL_SHADER);
            ok(f(self.0, ps), "SetPixelShader");
        }
    }

    /// A page with the decoder's constants, as phi sets them.
    fn draw_page(&self, page: &Page, cache: &Cache) {
        unsafe {
            let f: unsafe extern "system" fn(Ptr, u32, Ptr) -> HRESULT = m(self.0, DEV_SET_TEXTURE);
            f(self.0, 0, cache.texture);
            f(self.0, 1, cache.map);
            f(self.0, 2, cache.atlas);
            f(self.0, 3, cache.cover);
            f(self.0, 4, cache.words);
            // c202 the atlas and the scale, c203 the column map's height.
            let atl = [
                256.0f32,
                1.0 / 256.0,
                1.0 / cache.atlas_h as f32,
                cache.k,
                1.0 / 256.0,
                0.0,
                0.0,
                0.0,
            ];
            let fc: unsafe extern "system" fn(Ptr, u32, *const f32, u32) -> HRESULT =
                m(self.0, DEV_SET_PS_CONSTANT_F);
            fc(self.0, CFG + 2, atl.as_ptr(), 2);
            let ch = cache.cell_height as f32;
            let cfg = [ch, 1.0 / ch, 1.0 / GAMMA, GAMMA, cache.edge, 0.0, 0.0, 0.0];
            let f: unsafe extern "system" fn(Ptr, u32, *const f32, u32) -> HRESULT =
                m(self.0, DEV_SET_PS_CONSTANT_F);
            f(self.0, CFG, cfg.as_ptr(), 2);
            let f: unsafe extern "system" fn(Ptr, u32, u32, *const c_void, u32) -> HRESULT =
                m(self.0, DEV_DRAW_PRIMITIVE_UP);
            ok(
                f(
                    self.0,
                    4,
                    (page.verts.len() / 3) as u32,
                    page.verts.as_ptr() as *const c_void,
                    size_of::<Vertex>() as u32,
                ),
                "DrawPrimitiveUP",
            );
        }
    }
    fn scene(&self, begin: bool) {
        unsafe {
            let f: unsafe extern "system" fn(Ptr) -> HRESULT = m(
                self.0,
                if begin {
                    DEV_BEGIN_SCENE
                } else {
                    DEV_END_SCENE
                },
            );
            f(self.0);
        }
    }

    fn clear(&self) {
        unsafe {
            let f: unsafe extern "system" fn(
                Ptr,
                u32,
                *const c_void,
                u32,
                u32,
                f32,
                u32,
            ) -> HRESULT = m(self.0, DEV_CLEAR);
            // Colour only, to a value no shader writes.
            ok(
                f(self.0, 0, std::ptr::null(), 1, 0x00FF_00FF, 1.0, 0),
                "Clear",
            );
        }
    }

    fn query(&self, ty: u32) -> Ptr {
        unsafe {
            let f: unsafe extern "system" fn(Ptr, u32, *mut Ptr) -> HRESULT =
                m(self.0, DEV_CREATE_QUERY);
            let mut q: Ptr = std::ptr::null_mut();
            ok(f(self.0, ty, &mut q), "CreateQuery");
            q
        }
    }
}

fn el(stream: u16, offset: u16, ty: u8, usage: u8, index: u8) -> [u16; 4] {
    [
        stream,
        offset,
        ty as u16,
        usage as u16 | (index as u16) << 8,
    ]
}

fn issue(q: Ptr, flags: u32) {
    unsafe {
        let f: unsafe extern "system" fn(Ptr, u32) -> HRESULT = m(q, Q_ISSUE);
        f(q, flags);
    }
}

/// Wait for a query's result (flushing).
fn get<T: Default>(q: Ptr) -> T {
    let mut v = T::default();
    unsafe {
        let f: unsafe extern "system" fn(Ptr, *mut c_void, u32, u32) -> HRESULT = m(q, Q_GET_DATA);
        loop {
            match f(q, &mut v as *mut T as *mut c_void, size_of::<T>() as u32, 1) {
                0 => break,
                1 => std::thread::yield_now(),
                r => panic!("GetData: {:#x}", r as u32),
            }
        }
    }
    v
}

// ---- caches -------------------------------------------------------------

struct Cell {
    x: u32,
    y: u32,
    w: u32,
    /// The outline in cell texels, for the reference: as quadratics for
    /// the area, as chords for the distance.
    contours: Vec<Vec<glyph::Quad>>,
    lines: Vec<Vec<glyph::P>>,
}

struct Cache {
    name: String,
    cell_height: u32,
    cells: Vec<Cell>,
    texture: Ptr,
    /// The outline's radius in texels, 0 for none.
    edge: f32,
    /// Format v2 (shaders/decoder.hlsl): the column map (s1), the atlas (s2)
    /// and its height in texels, and grid steps a cache texel.
    map: Ptr,
    atlas: Ptr,
    atlas_h: u32,
    /// A byte a cache texel (stage 3): 0 empty, 255 full, else an edge
    /// with its 1x coverage after the gamma (atlas::cover_cell).
    cover: Ptr,
    /// A word a cache texel (stage 4, atlas::texel_words).
    words: Ptr,
    k: f32,
}

const CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789.,:;!?'\"()[]{}<>+-*/=%&#@$_|~^ěščřžýáíéůúňťď";
static CHAR_SET: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// The colour the text is drawn in, RRGGBB (--color): white unless said.
static COLOR: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// COLOR as the shader gets it, 0..1.
fn color() -> [f32; 3] {
    let c = COLOR.get().copied().unwrap_or(0xFF_FFFF);
    [16, 8, 0].map(|s| (c >> s & 255) as f32 / 255.0)
}

/// Caches of `font` at `px` laid out as the client does it (rows of the
/// cell height, cells as wide as the bitmap plus the outline's padding),
/// every cell v2's: the client's texture (stage 0) stays empty.
fn build(file: &str, px: u32, outline: u32, dev: &Device) -> Vec<Cache> {
    let data = std::fs::read(file).unwrap_or_else(|e| panic!("{}: {}", file, e));
    let face = ttf_parser::Face::parse(&data, 0).expect("font");
    let s = px as f32 / face.units_per_em() as f32;
    let ascent = (face.ascender() as f32 * s).round();
    let ch = px + 2 * outline;
    let edge = atlas::outline_radius(outline, OUTLINE_EXTRA);
    let name = format!(
        "{} {}px outline {}",
        PathBuf::from(file).file_name().unwrap().to_string_lossy(),
        px,
        outline
    );
    // Format v2: one grid scale for the font (its box onto 4095 steps), so
    // cache texels to grid steps is one constant a draw.
    let bb = face.global_bounding_box();
    let side = ((bb.x_max - bb.x_min) as f32).max((bb.y_max - bb.y_min) as f32);
    let grid_scale = (atlas::GRID - 1.0) / side;
    const NO_GLYPH: [f32; 4] = [-1.0, 0.0, 0.0, 0.0];
    let (mut caches, mut cells, mut texels) = (Vec::new(), Vec::new(), vec![0u16; 256 * 256]);
    let (mut records, mut map) = (Vec::<[f32; 4]>::new(), vec![NO_GLYPH; 256 * 256]);
    // Two bytes a texel for an outlined font (fill, alpha), one for a plain.
    let per = if outline > 0 { 2 } else { 1 };
    let blank: Vec<u8> = if per == 2 {
        [atlas::NOT_OURS, 0].repeat(256 * 256)
    } else {
        vec![atlas::NOT_OURS; 256 * 256]
    };
    let mut cover = blank.clone();
    let mut texel_words = vec![[0u16; 4]; 256 * 256];
    let (mut x, mut row, mut ns) = (0u32, 0u32, 0u128);
    let mut flush = |cells: &mut Vec<Cell>,
                     texels: &mut Vec<u16>,
                     records: &mut Vec<[f32; 4]>,
                     map: &mut Vec<[f32; 4]>,
                     cover: &mut Vec<u8>,
                     texel_words: &mut Vec<[u16; 4]>| {
        if !cells.is_empty() {
            let texture = dev.create_texture(texels);
            let atlas_h = records.len().div_ceil(256).max(1) as u32;
            records.resize(256 * atlas_h as usize, [0.0; 4]);
            caches.push(Cache {
                name: name.clone(),
                cell_height: ch,
                cells: std::mem::take(cells),
                texture,
                edge,
                map: dev.create_texture_f32(256, 256, map),
                atlas: dev.create_texture_f32(256, atlas_h, records),
                cover: dev.create_texture_u8(cover, per),
                words: dev.create_texture_u16x4(texel_words),
                atlas_h,
                k: grid_scale / s,
            });
            texels.fill(0);
            records.clear();
            map.fill(NO_GLYPH);
            cover.copy_from_slice(&blank);
            texel_words.fill([0; 4]);
        }
    };
    for c in CHAR_SET.get().map_or(CHARS, String::as_str).chars() {
        let Some(g) = glyph::outline(&data, c as u32) else {
            continue;
        };
        let w = ((g.bbox[2] * s).ceil() - (g.bbox[0] * s).floor()).max(1.0) as u32 + 2 * outline;
        if x + w > 256 {
            (x, row) = (0, row + 1);
        }
        if (row + 1) * ch > 256 {
            flush(
                &mut cells,
                &mut texels,
                &mut records,
                &mut map,
                &mut cover,
                &mut texel_words,
            );
            (x, row) = (0, 0);
        }
        let y0 = row * ch;
        let top = (ascent - (g.bbox[3] * s).ceil()).max(0.0) as u32;
        let at = glyph::Placement {
            pixel_size: px,
            dx: outline,
            top: top + outline,
        };
        let contours = g.placed(&at);
        let lines = coverage::polylines(&contours);
        // Format v2: the record into the atlas, and every texel column of
        // the cell pointing at it with where its grid starts. A cell texel
        // X is the outline's x * s - left + dx, a texel Y (down) is top -
        // y * s + at.top, so grid steps are (X - ox) * k and (oy - Y) * k.
        let t = std::time::Instant::now();
        if let Some(r) = atlas::encode(&g.contours, Some(grid_scale), outline > 0) {
            let (left, topbox) = ((g.bbox[0] * s).floor(), (g.bbox[3] * s).ceil());
            let texel = [
                records.len() as f32,
                r.texels[0][1] * 16.0 + r.texels[0][0] - 1.0,
                x as f32 + at.dx as f32 - left + r.origin[0] * s,
                y0 as f32 + at.top as f32 + topbox - r.origin[1] * s,
            ];
            records.extend_from_slice(&r.texels);
            for cx in x..x + w {
                map[(row * 256 + cx) as usize] = texel;
            }
            let k = grid_scale / s;
            let at = [texel[2] - x as f32, texel[3] - y0 as f32];
            let (bytes, words) = atlas::cell(&r, w, ch, at, k, edge, 1.0 / GAMMA);
            for (i, word) in words.into_iter().enumerate() {
                texel_words[((y0 + i as u32 / w) * 256 + x + i as u32 % w) as usize] = word;
            }
            for (j, row) in bytes.chunks_exact(per * w as usize).enumerate() {
                let i = per * ((y0 as usize + j) * 256 + x as usize);
                cover[i..i + row.len()].copy_from_slice(row);
            }
        }
        ns += t.elapsed().as_nanos();
        cells.push(Cell {
            x,
            y: y0,
            w,
            contours,
            lines,
        });
        x += w;
    }
    flush(
        &mut cells,
        &mut texels,
        &mut records,
        &mut map,
        &mut cover,
        &mut texel_words,
    );
    let n: usize = caches.iter().map(|c| c.cells.len()).sum();
    eprintln!(
        "{}: {} cells, {} caches, {:.3} ms a cell",
        name,
        n,
        caches.len(),
        ns as f64 / 1e6 / n.max(1) as f64
    );
    caches
}

// ---- the exact reference ------------------------------------------------

/// A page's texels as they should be where the probe says the shaders
/// sampled (p and fw in texels): the glyph's area in the pixel's box under
/// the nonzero rule (coverage::coverage on the box as one texel), the true
/// distance for the outline, through the decoder's definitions but none of
/// its shortcuts; None where nothing is drawn.
fn exact(cache: &Cache, probe: &[[f32; 4]]) -> Vec<Option<[f32; 4]>> {
    let mut owner = vec![u16::MAX; 256 * 256];
    for (k, c) in cache.cells.iter().enumerate() {
        for y in c.y..c.y + cache.cell_height {
            owner[(y * 256 + c.x) as usize..(y * 256 + c.x + c.w) as usize].fill(k as u16);
        }
    }
    let g = 1.0 / GAMMA;
    // The decoder's alphas (decoder.hlsl, tone): a light text's and black's.
    let rgb = color();
    let luma = 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
    let black = |c: f32| 1.0 - (1.0 - c).powf(g);
    let tone = |c: f32| black(c) + (c.powf(g) - black(c)) * luma;
    let tint = |t: [f32; 4]| Some([t[0] * rgb[0], t[1] * rgb[1], t[2] * rgb[2], t[3]]);
    probe
        .iter()
        .map(|t| {
            // The clear colour has fw 0.
            if t[3] <= 0.0 || !(0.0..256.0).contains(&t[0]) || !(0.0..256.0).contains(&t[1]) {
                return None;
            }
            let cell = cache
                .cells
                .get(owner[t[1] as usize * 256 + t[0] as usize] as usize)?;
            let (cx, cy) = (t[0] - cell.x as f32, t[1] - cell.y as f32);
            let to_box = |p: glyph::P| [(p[0] - cx) / t[2] + 0.5, (p[1] - cy) / t[3] + 0.5];
            let boxed: Vec<Vec<glyph::Quad>> = cell
                .contours
                .iter()
                .map(|c| c.iter().map(|q| q.map(to_box)).collect())
                .collect();
            let fill = coverage::coverage(&coverage::polylines(&boxed), 1, 1)[0];
            if cache.edge == 0.0 {
                return tint([1.0, 1.0, 1.0, tone(fill)]);
            }
            let d = cell
                .lines
                .iter()
                .flat_map(|l| (0..l.len()).map(move |s| (l[s], l[(s + 1) % l.len()])))
                .map(|(a, b)| {
                    let (e, q) = ([b[0] - a[0], b[1] - a[1]], [cx - a[0], cy - a[1]]);
                    let len2 = e[0] * e[0] + e[1] * e[1];
                    let u = if len2 > 0.0 {
                        ((q[0] * e[0] + q[1] * e[1]) / len2).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    (q[0] - e[0] * u).hypot(q[1] - e[1] * u)
                })
                .fold(f32::MAX, f32::min);
            let edge = ((cache.edge - d) / (0.5 * (t[2] + t[3])) + 0.5).clamp(0.0, 1.0);
            tint([fill, fill, fill, black(edge.max(fill))])
        })
        .collect()
}

// ---- pages --------------------------------------------------------------

struct Page {
    cache: usize,
    scale: f32,
    verts: Vec<Vertex>,
    /// Pixels the quads cover.
    pixels: f64,
}

fn page(cache: usize, c: &Cache, scale: f32, size: u32) -> Page {
    let mut verts = Vec::new();
    let mut pixels = 0.0;
    let ndc = |x: f32, y: f32| {
        [
            (x - 0.5) / size as f32 * 2.0 - 1.0,
            1.0 - (y - 0.5) / size as f32 * 2.0,
            0.5,
            1.0,
        ]
    };
    for cell in &c.cells {
        let (x0, y0) = (cell.x as f32 * scale, cell.y as f32 * scale);
        let (x1, y1) = (
            (cell.x + cell.w) as f32 * scale,
            (cell.y + c.cell_height) as f32 * scale,
        );
        let (u0, v0) = (cell.x as f32 / 256.0, cell.y as f32 / 256.0);
        let (u1, v1) = (
            (cell.x + cell.w) as f32 / 256.0,
            (cell.y + c.cell_height) as f32 / 256.0,
        );
        let v = |x: f32, y: f32, u: f32, w: f32| Vertex {
            pos: ndc(x, y),
            color: 0xFF00_0000 | COLOR.get().copied().unwrap_or(0xFF_FFFF),
            uv: [u, w],
        };
        let (a, b, cc, d) = (
            v(x0, y0, u0, v0),
            v(x1, y0, u1, v0),
            v(x0, y1, u0, v1),
            v(x1, y1, u1, v1),
        );
        verts.extend([a, b, cc, b, d, cc]);
        pixels += ((x1 - x0) * (y1 - y0)) as f64;
    }
    Page {
        cache,
        scale,
        verts,
        pixels,
    }
}

// ---- output -------------------------------------------------------------

/// An 8-bit RGBA PNG, stored (uncompressed) deflate.
fn png(path: &PathBuf, size: u32, px: impl Fn(usize) -> [u8; 4]) {
    fn crc(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    let n = size as usize;
    let mut raw = Vec::with_capacity(n * (4 * n + 1));
    for y in 0..n {
        raw.push(0);
        for x in 0..n {
            raw.extend(px(y * n + x));
        }
    }
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, c) in chunks.iter().enumerate() {
        z.push((i + 1 == chunks.len()) as u8);
        z.extend((c.len() as u16).to_le_bytes());
        z.extend((!(c.len() as u16)).to_le_bytes());
        z.extend(*c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend((b << 16 | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |ty: &[u8], data: &[u8]| {
        out.extend((data.len() as u32).to_be_bytes());
        let mut c = ty.to_vec();
        c.extend(data);
        out.extend(&c);
        out.extend(crc(&c).to_be_bytes());
    };
    let mut ihdr = size.to_be_bytes().to_vec();
    ihdr.extend(size.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    std::fs::write(path, out).unwrap_or_else(|e| panic!("{}: {}", path.display(), e));
}

/// How far a shader is from the reference over text texels (either side
/// drew ink or outline): how many, the summed difference, how many differ
/// by over 2/255 (visible) and 16/255 (gross).
#[derive(Default, Clone)]
struct Error {
    text: usize,
    sum: f64,
    visible: usize,
    gross: usize,
}

impl Error {
    fn add(&mut self, d: f32) {
        self.text += 1;
        self.sum += d as f64;
        self.visible += (d > 2.0 / 255.0) as usize;
        self.gross += (d > 16.0 / 255.0) as usize;
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let n = self.text.max(1) as f64;
        write!(
            f,
            "over {} text texels mean {:.5}, {:.2} % over 2/255, {:.3} % over 16/255",
            self.text,
            self.sum / n,
            100.0 * self.visible as f64 / n,
            100.0 * self.gross as f64 / n
        )
    }
}

fn to8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A texel as the client would show it: over mid grey, by its alpha (the
/// fill white in RGB, an outline black); the clear colour (alpha 0) stays
/// grey.
fn over_grey(t: [f32; 4]) -> [u8; 4] {
    let a = t[3].clamp(0.0, 1.0);
    let c = |v: f32| to8(v * a + 0.5 * (1.0 - a));
    [c(t[0]), c(t[1]), c(t[2]), 255]
}

// ---- main ---------------------------------------------------------------

fn main() {
    let mut out = PathBuf::from("bench-out");
    let mut shaders: Vec<(String, Vec<u8>)> = Vec::new();
    let mut fonts: Vec<(String, u32, u32)> = Vec::new();
    let mut scales = vec![0.5f32, 1.0, 1.5, 2.0];
    let (mut reps, mut rounds, mut time) = (10usize, 7usize, true);
    // Pages written as PNGs for every shader (NAME-pageN.png).
    let mut shown: Vec<usize> = Vec::new();
    // Per scale, the most texels over 16/255 against the reference, in %.
    let mut limits: Vec<(f32, f64)> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| panic!("{} wants a value", a));
        match a.as_str() {
            "--out" => out = PathBuf::from(val()),
            "--ps" => {
                let v = val();
                // An older bench's "@cells" after the file is the same cells now.
                let (name, file) = v.split_once('=').expect("--ps NAME=FILE");
                let file = file.split('@').next().unwrap();
                shaders.push((
                    name.into(),
                    std::fs::read(file).unwrap_or_else(|e| panic!("{}: {}", file, e)),
                ));
            }
            "--font" => {
                let v = val();
                let mut it = v.rsplitn(3, ':');
                let outline = it.next().unwrap().parse().expect("outline");
                let px = it.next().expect("FILE:PX:OUTLINE").parse().expect("px");
                fonts.push((it.next().expect("FILE:PX:OUTLINE").into(), px, outline));
            }
            "--scales" => {
                scales = val()
                    .split(',')
                    .map(|s| s.parse().expect("scale"))
                    .collect()
            }
            "--reps" => reps = val().parse().expect("reps"),
            "--rounds" => rounds = val().parse().expect("rounds"),
            "--no-time" => time = false,
            "--chars" => CHAR_SET.set(val()).unwrap(),
            "--color" => COLOR
                .set(u32::from_str_radix(&val(), 16).expect("--color RRGGBB"))
                .unwrap(),
            "--pages" => shown = val().split(',').map(|s| s.parse().expect("page")).collect(),
            "--max-gross" => {
                limits = val()
                    .split(',')
                    .map(|s| {
                        let (scale, pct) = s.split_once(':').expect("--max-gross SCALE:PCT");
                        (scale.parse().expect("scale"), pct.parse().expect("pct"))
                    })
                    .collect()
            }
            _ => panic!("unknown argument {}", a),
        }
    }
    assert!(
        !shaders.is_empty() && !fonts.is_empty(),
        "want --ps and --font"
    );
    std::fs::create_dir_all(&out).expect("--out");

    let dev = Device::new();
    dev.fixed_state();
    let size = (256.0 * scales.iter().cloned().fold(1.0, f32::max)).ceil() as u32;
    let (rt, sys) = dev.render_target(size);
    let pss: Vec<Ptr> = shaders
        .iter()
        .map(|(_, code)| dev.pixel_shader(code))
        .collect();
    let caches: Vec<Cache> = fonts
        .iter()
        .flat_map(|(f, px, o)| build(f, *px, *o, &dev))
        .collect();
    let pages: Vec<Page> = caches
        .iter()
        .enumerate()
        .flat_map(|(i, c)| scales.iter().map(move |&s| page(i, c, s, size)))
        .collect();
    let pixels: f64 = pages.iter().map(|p| p.pixels).sum();
    let mut report = String::new();
    let _ = writeln!(
        report,
        "{} pages ({} caches x scales {:?}), {:.2} Mpx a pass",
        pages.len(),
        caches.len(),
        scales,
        pixels / 1e6
    );

    let uv = dev.pixel_shader(UV_PS);
    let render = |ps: Ptr, p: &Page| {
        dev.set_ps(ps);
        dev.clear();
        dev.scene(true);
        dev.draw_page(p, &caches[p.cache]);
        dev.scene(false);
        dev.read(rt, sys, size)
    };
    let largest =
        |a: &[f32; 4], b: &[f32; 4]| (0..4).map(|c| (a[c] - b[c]).abs()).fold(0.0, f32::max);
    // Per shader, overall and by scale: against the exact reference, and
    // against the first shader (with the first page that differs).
    let mut vs_exact =
        vec![(Error::default(), vec![Error::default(); scales.len()]); shaders.len()];
    let mut vs_first = vs_exact.clone();
    let mut first: Vec<Option<usize>> = vec![None; shaders.len()];
    for (pi, p) in pages.iter().enumerate() {
        let sc = scales.iter().position(|&s| s == p.scale).unwrap();
        let truth = exact(&caches[p.cache], &render(uv, p));
        let reference = render(pss[0], p);
        if shown.contains(&pi) {
            png(&out.join(format!("exact-page{}.png", pi)), size, |i| {
                over_grey(truth[i].unwrap_or([0.0; 4]))
            });
            let _ = writeln!(
                report,
                "page {}: {} at {}x",
                pi, caches[p.cache].name, p.scale
            );
        }
        for (si, (name, _)) in shaders.iter().enumerate() {
            let img = if si == 0 {
                reference.clone()
            } else {
                render(pss[si], p)
            };
            for (a, t) in img.iter().zip(&truth) {
                // Text texels: either side drew ink or outline.
                if let Some(t) = t.filter(|t| t[3] > 0.0 || a[3] > 0.0) {
                    let d = largest(a, &t);
                    vs_exact[si].0.add(d);
                    vs_exact[si].1[sc].add(d);
                }
            }
            if shown.contains(&pi) {
                png(&out.join(format!("{}-page{}.png", name, pi)), size, |i| {
                    over_grey(img[i])
                });
                // 8 times over: 32/255 shows white.
                png(
                    &out.join(format!("{}-page{}-error.png", name, pi)),
                    size,
                    |i| {
                        let v = truth[i].map_or(0, |t| to8(largest(&img[i], &t) * 8.0));
                        [v, v, v, 255]
                    },
                );
            }
            if si == 0 {
                continue;
            }
            let mut differ = false;
            for (a, b) in img
                .iter()
                .zip(&reference)
                .filter(|(a, b)| a[3] > 0.0 || b[3] > 0.0)
            {
                let d = largest(a, b);
                differ |= d > 0.0;
                vs_first[si].0.add(d);
                vs_first[si].1[sc].add(d);
            }
            if differ && first[si].is_none() {
                first[si] = Some(pi);
                png(
                    &out.join(format!("{}-page{}-diff.png", name, pi)),
                    size,
                    |i| {
                        let g = to8(reference[i][3]) / 4;
                        if largest(&img[i], &reference[i]) > 0.0 {
                            [255, 0, 255, 255]
                        } else {
                            [g, g, g, 255]
                        }
                    },
                );
            }
        }
    }
    for (si, (name, _)) in shaders.iter().enumerate() {
        let mut lines = vec![(format!("exact {}", name), &vs_exact[si])];
        if si > 0 {
            let at = first[si].map_or(String::new(), |p| format!(" (first: page {})", p));
            lines.push((
                format!("check {} vs {}{}", name, shaders[0].0, at),
                &vs_first[si],
            ));
        }
        for (what, (all, by_scale)) in lines {
            let _ = writeln!(report, "{}: {}", what, all);
            for (s, e) in scales.iter().zip(by_scale) {
                let _ = writeln!(report, "  at {}x: {}", s, e);
            }
        }
    }

    if time {
        let freq = dev.query(12);
        let (t0, t1) = (dev.query(10), dev.query(10));
        let mut ms: Vec<Vec<f64>> = vec![Vec::new(); shaders.len()];
        for _ in 0..rounds {
            for (si, &ps) in pss.iter().enumerate() {
                dev.set_ps(ps);
                dev.scene(true);
                issue(t0, 1);
                for _ in 0..reps {
                    for p in &pages {
                        dev.draw_page(p, &caches[p.cache]);
                    }
                }
                issue(t1, 1);
                issue(freq, 1);
                dev.scene(false);
                let (a, b, f) = (get::<u64>(t0), get::<u64>(t1), get::<u64>(freq));
                ms[si].push((b - a) as f64 * 1e3 / f as f64 / reps as f64);
            }
        }
        for v in &mut ms {
            v.sort_by(f64::total_cmp);
        }
        let base = ms[0][ms[0].len() / 2];
        for (si, (name, code)) in shaders.iter().enumerate() {
            let med = ms[si][ms[si].len() / 2];
            let _ = writeln!(
                report,
                "time {:<24} {:8.4} ms a pass ({:.4}..{:.4}), {:6.3} ns/px, {:5.1} % of {}, {} bytes",
                name,
                med,
                ms[si][0],
                ms[si][ms[si].len() - 1],
                med * 1e6 / pixels,
                100.0 * med / base,
                shaders[0].0,
                code.len()
            );
        }
    }
    print!("{}", report);
    let _ = std::fs::write(out.join("report.txt"), report);

    let mut over = 0;
    for (si, (name, _)) in shaders.iter().enumerate() {
        for &(scale, max) in &limits {
            let sc = scales
                .iter()
                .position(|&s| s == scale)
                .unwrap_or_else(|| panic!("--max-gross {}: not among --scales", scale));
            let e = &vs_exact[si].1[sc];
            let pct = 100.0 * e.gross as f64 / e.text.max(1) as f64;
            if pct > max {
                eprintln!(
                    "{} at {}x: {:.3} % over 16/255, the limit is {} %",
                    name, scale, pct, max
                );
                over += 1;
            }
        }
    }
    if over > 0 {
        std::process::exit(1);
    }
}
