//! φ: the device holds the image of the client's state, not the state
//! (docs/design.md, item 3). The client's state s is its pixel shader,
//! whether its vertex shader is a vs_3_0, and the texture on stage 0:
//!
//!   φ(ps, vs, t) = (decoder, c200 = cfg(t))  if t is a known font's glyph
//!                                             cache and vs a vs_3_0
//!                = (ps, c200 untouched)       otherwise
//!
//! Setters update s and call the device only where φ(s) changed; getters
//! answer with s; draws pass untouched. Caches and vs_3_0s are told apart
//! by their vtables (lib::Class). To hold:
//! - c200 is ours (the client reads c0..c7); a client write there drops
//!   our copy until the next emit.
//! - s keeps its own reference to the client's shader, which the client
//!   may release while ours stands in.
//! - Reset and a state block's Apply change the device behind our back:
//!   both read s back. Inside BeginStateBlock φ is the identity, so a
//!   recorded block holds the client's state.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};

use crate::{Class, HRESULT, Ptr, dev_orig, draw, fonts, log, payload, settings, texture};

const DEV_RESET: usize = 16;
const DEV_CREATE_STATE_BLOCK: usize = 59;
const DEV_BEGIN_STATE_BLOCK: usize = 60;
const DEV_END_STATE_BLOCK: usize = 61;
const DEV_GET_TEXTURE: usize = 64;
const DEV_SET_TEXTURE: usize = 65;
const DEV_CREATE_VERTEX_SHADER: usize = 91;
const DEV_SET_VERTEX_SHADER: usize = 92;
const DEV_GET_VERTEX_SHADER: usize = 93;
const DEV_SET_PIXEL_SHADER: usize = 107;
const DEV_GET_PIXEL_SHADER: usize = 108;
const DEV_SET_PS_CONSTANT_F: usize = 109;
const SB_APPLY: usize = 5;
/// Entries copied for shaders and state blocks (see lib's DEV_VTABLE).
const VTABLE: usize = 64;

const D3DERR_INVALIDCALL: HRESULT = 0x8876086Cu32 as i32;
const VS_3_0: u32 = 0xFFFE0300;

/// The decoder's constants (decoder.hlsl): c200 cell height, 1 / cell height,
/// 1 / gamma; c201 outline radius in texels, the words' path in colour;
/// c202 the atlas; c203 the column map.
const CFG: u32 = 200;
const CFG_REGS: u32 = 4;
const UNKNOWN: u32 = f32::NAN.to_bits();

/// Vertex shaders that are vs_3_0.
static VS3: Class = Class::new();
/// State blocks, for Apply.
static BLOCKS: Class = Class::new();

/// The device (one; the client makes one).
static DEV: AtomicUsize = AtomicUsize::new(0);
/// s: the client's pixel shader (with a reference of ours), whether its
/// vertex shader is a vs_3_0, the texture on stage 0.
static PS: AtomicUsize = AtomicUsize::new(0);
static VS_OK: AtomicBool = AtomicBool::new(false);
static TEX0: AtomicUsize = AtomicUsize::new(0);
/// TEX0's cell height when the decoder can take it, else 0, and its
/// font's outline radius in texels (f32 bits).
static CELL: AtomicU32 = AtomicU32::new(0);
static EDGE: AtomicU32 = AtomicU32::new(0);
/// TEX0's font header when the curve decoder of v2 takes it, else 0
/// (the textures it needs come from v2::bind at each emit: the atlas may
/// have grown into another texture since).
static V2_HEADER: AtomicU32 = AtomicU32::new(0);
/// The stages past 0 the curve decoder takes: 1 to 4.
const STAGES: usize = 4;
/// The client's textures on stages 1 to 4 (with a reference of ours: the
/// device may not hold them while the curve decoder has its own there),
/// and what the device holds there.
static CLIENT_TEX: [AtomicUsize; STAGES] = [const { AtomicUsize::new(0) }; STAGES];
static DEV_TEX: [AtomicUsize; STAGES] = [const { AtomicUsize::new(0) }; STAGES];
/// What the device holds: its pixel shader, and c200..c201 (UNKNOWN:
/// anything).
static DEV_PS: AtomicUsize = AtomicUsize::new(0);
static DEV_CFG: [AtomicU32; 16] = [const { AtomicU32::new(UNKNOWN) }; 16];
static RECORDING: AtomicBool = AtomicBool::new(false);

unsafe fn add_ref(obj: usize) {
    if obj != 0 {
        unsafe {
            let vt = *(obj as *const *const usize);
            let f: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vt.add(1));
            f(obj);
        }
    }
}

unsafe fn release(obj: usize) {
    if obj != 0 {
        unsafe {
            let vt = *(obj as *const *const usize);
            let f: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vt.add(2));
            f(obj);
        }
    }
}

/// s's pixel shader becomes `ps` (a reference already taken for it).
fn hold(ps: usize) {
    unsafe { release(PS.swap(ps, Relaxed)) };
}

fn set_ps_raw(dev: Ptr, ps: usize) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, usize) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_SET_PIXEL_SHADER)) };
    unsafe { f(dev, ps) }
}

fn set_tex_raw(dev: Ptr, stage: u32, tex: usize) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, u32, usize) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_SET_TEXTURE)) };
    unsafe { f(dev, stage, tex) }
}

/// Bring the device to φ(s), calling it only for what differs.
fn emit(dev: Ptr) {
    let ch = f32::from_bits(CELL.load(Relaxed));
    let text = ch > 0.0 && VS_OK.load(Relaxed);
    let header = V2_HEADER.load(Relaxed);
    let bind = (text && header != 0)
        .then(|| crate::v2::bind(TEX0.load(Relaxed), header))
        .flatten();
    let ours = bind.as_ref().map_or(0, |_| draw::decoder());
    let ps = if ours != 0 { ours } else { PS.load(Relaxed) };
    if DEV_PS.swap(ps, Relaxed) != ps {
        set_ps_raw(dev, ps);
    }
    // Stages 1 to 4: the column map, the atlas, the cover and the texel
    // words while the curve decoder draws, the client's textures otherwise.
    let want = match &bind {
        Some(b) => [b.map, b.atlas, b.cover, b.words],
        None => std::array::from_fn(|i| CLIENT_TEX[i].load(Relaxed)),
    };
    for (i, &t) in want.iter().enumerate() {
        if DEV_TEX[i].swap(t, Relaxed) != t {
            set_tex_raw(dev, i as u32 + 1, t);
        }
    }
    if ours == 0 {
        return;
    }
    let (atlas_rows, k, map_rows) = bind
        .as_ref()
        .map_or((1, 0.0, 1), |b| (b.atlas_rows, b.k, b.map_rows));
    let cfg = [
        ch,
        1.0 / ch,
        1.0 / settings::GAMMA.get(),
        settings::GAMMA.get(),
        f32::from_bits(EDGE.load(Relaxed)),
        0.0,
        0.0,
        // The words' path in colour (decoder.hlsl).
        (settings::WORDS.get() >= 2.0) as u32 as f32,
        256.0,
        1.0 / 256.0,
        1.0 / atlas_rows as f32,
        k,
        1.0 / map_rows as f32,
        0.0,
        0.0,
        0.0,
    ];
    if DEV_CFG
        .iter()
        .zip(&cfg)
        .any(|(d, c)| d.load(Relaxed) != c.to_bits())
    {
        let f: unsafe extern "system" fn(Ptr, u32, *const f32, u32) -> HRESULT =
            unsafe { std::mem::transmute(dev_orig(DEV_SET_PS_CONSTANT_F)) };
        unsafe { f(dev, CFG, cfg.as_ptr(), CFG_REGS) };
        for (d, c) in DEV_CFG.iter().zip(&cfg) {
            d.store(c.to_bits(), Relaxed);
        }
    }
}

/// The cell height the decoder needs for `tex`, or 0 to leave the draw to
/// the client's bitmap (not a glyph cache, or its font not known yet).
/// Brings the texture's contents up to date on the way: a bind is the
/// last moment before a draw.
fn cell_of(tex: usize) -> f32 {
    if !texture::TEX.has(tex as Ptr) {
        return 0.0;
    }
    let _trace = crate::trace::span(crate::trace::Kind::Bind);
    let owner = || {
        fonts::owners()
            .lock()
            .unwrap()
            .get(&tex)
            .map(|o| (o.header, o.cell_height, o.flags))
    };
    let owner = owner().or_else(|| {
        fonts::adopt(false);
        owner().or_else(|| {
            fonts::drawn_orphan(tex);
            None
        })
    });
    let Some((header, ch, flags)) = owner else {
        return 0.0;
    };
    // A font v2 does not take (monochrome) keeps the client's bitmaps.
    if !crate::v2::eligible(flags) {
        return 0.0;
    }
    EDGE.store(crate::v2::radius_of(flags).to_bits(), Relaxed);
    V2_HEADER.store(header, Relaxed);
    if texture::stale(tex) || payload::needs_refresh(tex, header) {
        texture::refresh(tex);
    }
    ch as f32
}

/// Read s back from the device, after something other than the setters
/// changed its state (Reset, a state block), and bring it to φ(s).
fn resync(dev: Ptr) {
    type GetPtr = unsafe extern "system" fn(Ptr, *mut usize) -> HRESULT;
    unsafe {
        let get_ps: GetPtr = std::mem::transmute(dev_orig(DEV_GET_PIXEL_SHADER));
        let get_vs: GetPtr = std::mem::transmute(dev_orig(DEV_GET_VERTEX_SHADER));
        let get_tex: unsafe extern "system" fn(Ptr, u32, *mut usize) -> HRESULT =
            std::mem::transmute(dev_orig(DEV_GET_TEXTURE));
        let (mut ps, mut vs, mut tex) = (0usize, 0usize, 0usize);
        get_ps(dev, &mut ps);
        get_vs(dev, &mut vs);
        get_tex(dev, 0, &mut tex);
        DEV_PS.store(ps, Relaxed);
        // The device holding one of ours (a block captured it) leaves s's
        // shader as it was.
        if draw::ours(ps) {
            release(ps);
        } else {
            hold(ps);
        }
        VS_OK.store(VS3.has(vs as Ptr), Relaxed);
        release(vs);
        tex0(tex);
        release(tex);
        // Stages 1 to 4: the device's are s's, unless they are ours (a
        // block captured them), which leaves s's as they were.
        for (i, (client, held)) in CLIENT_TEX.iter().zip(&DEV_TEX).enumerate() {
            let mut t = 0usize;
            get_tex(dev, i as u32 + 1, &mut t);
            held.store(t, Relaxed);
            if crate::v2::owns(t) {
                release(t);
            } else {
                release(client.swap(t, Relaxed));
            }
        }
    }
    for d in &DEV_CFG {
        d.store(UNKNOWN, Relaxed);
    }
    emit(dev);
}

/// s's texture on stage 0 becomes `tex`.
fn tex0(tex: usize) {
    TEX0.store(tex, Relaxed);
    V2_HEADER.store(0, Relaxed);
    crate::trace::text(texture::TEX.has(tex as Ptr));
    CELL.store(cell_of(tex).to_bits(), Relaxed);
}

pub fn device(dev: Ptr) {
    DEV.store(dev as usize, Relaxed);
}

pub fn hooks() -> [(usize, usize); 11] {
    [
        (DEV_RESET, reset as *const () as usize),
        (
            DEV_CREATE_STATE_BLOCK,
            create_state_block as *const () as usize,
        ),
        (
            DEV_BEGIN_STATE_BLOCK,
            begin_state_block as *const () as usize,
        ),
        (DEV_END_STATE_BLOCK, end_state_block as *const () as usize),
        (DEV_SET_TEXTURE, set_texture as *const () as usize),
        (DEV_GET_TEXTURE, get_texture as *const () as usize),
        (
            DEV_CREATE_VERTEX_SHADER,
            create_vertex_shader as *const () as usize,
        ),
        (
            DEV_SET_VERTEX_SHADER,
            set_vertex_shader as *const () as usize,
        ),
        (DEV_SET_PIXEL_SHADER, set_pixel_shader as *const () as usize),
        (DEV_GET_PIXEL_SHADER, get_pixel_shader as *const () as usize),
        (
            DEV_SET_PS_CONSTANT_F,
            set_ps_constant_f as *const () as usize,
        ),
    ]
}

// ---- the setters ------------------------------------------------------

unsafe extern "system" fn set_texture(dev: Ptr, stage: u32, tex: Ptr) -> HRESULT {
    // Stages 1 to 4 are s's (held), the device gets them through emit: the
    // curve decoder may have its own there.
    if (1..=STAGES as u32).contains(&stage) && !RECORDING.load(Relaxed) {
        unsafe {
            add_ref(tex as usize);
            release(CLIENT_TEX[stage as usize - 1].swap(tex as usize, Relaxed));
        }
        emit(dev);
        return 0;
    }
    let f: unsafe extern "system" fn(Ptr, u32, Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_SET_TEXTURE)) };
    let r = unsafe { f(dev, stage, tex) };
    if stage == 0 && !RECORDING.load(Relaxed) && TEX0.load(Relaxed) != tex as usize {
        tex0(tex as usize);
        emit(dev);
    }
    r
}

/// The client asks for its own textures on stages 1 to 4, whatever the
/// device holds there.
unsafe extern "system" fn get_texture(dev: Ptr, stage: u32, out: *mut usize) -> HRESULT {
    if (1..=STAGES as u32).contains(&stage) && !out.is_null() {
        let t = CLIENT_TEX[stage as usize - 1].load(Relaxed);
        unsafe {
            add_ref(t);
            *out = t;
        }
        return 0;
    }
    let f: unsafe extern "system" fn(Ptr, u32, *mut usize) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_GET_TEXTURE)) };
    unsafe { f(dev, stage, out) }
}

unsafe extern "system" fn set_pixel_shader(dev: Ptr, ps: usize) -> HRESULT {
    if RECORDING.load(Relaxed) {
        return set_ps_raw(dev, ps);
    }
    unsafe { add_ref(ps) };
    hold(ps);
    emit(dev);
    0
}

unsafe extern "system" fn get_pixel_shader(_dev: Ptr, out: *mut usize) -> HRESULT {
    if out.is_null() {
        return D3DERR_INVALIDCALL;
    }
    let ps = PS.load(Relaxed);
    unsafe {
        add_ref(ps);
        *out = ps;
    }
    0
}

unsafe extern "system" fn set_vertex_shader(dev: Ptr, vs: Ptr) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_SET_VERTEX_SHADER)) };
    let r = unsafe { f(dev, vs) };
    if !RECORDING.load(Relaxed) {
        VS_OK.store(VS3.has(vs), Relaxed);
        emit(dev);
    }
    r
}

unsafe extern "system" fn create_vertex_shader(
    dev: Ptr,
    code: *const u32,
    out: *mut Ptr,
) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, *const u32, *mut Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_CREATE_VERTEX_SHADER)) };
    let r = unsafe { f(dev, code, out) };
    if r >= 0 && !code.is_null() && !out.is_null() && unsafe { *code } == VS_3_0 {
        let vs = unsafe { *out };
        if !vs.is_null() && !unsafe { VS3.adopt(vs, VTABLE, &[]) } {
            log("vs_3_0 of another class: not tagged");
        }
    }
    r
}

unsafe extern "system" fn set_ps_constant_f(
    dev: Ptr,
    start: u32,
    data: *const f32,
    count: u32,
) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, u32, *const f32, u32) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_SET_PS_CONSTANT_F)) };
    if !RECORDING.load(Relaxed) && start < CFG + CFG_REGS && CFG < start.saturating_add(count) {
        static TOLD: AtomicBool = AtomicBool::new(false);
        if !TOLD.swap(true, Relaxed) {
            log(&format!(
                "the client writes c{}..c{}, over our c{}",
                start,
                start + count - 1,
                CFG
            ));
        }
        for d in &DEV_CFG {
            d.store(UNKNOWN, Relaxed);
        }
    }
    unsafe { f(dev, start, data, count) }
}

// ---- what changes the state behind the setters ------------------------

unsafe extern "system" fn reset(dev: Ptr, params: Ptr) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_RESET)) };
    crate::trace::reset();
    let r = unsafe { f(dev, params) };
    log(&format!("Reset -> {:#x}", r));
    RECORDING.store(false, Relaxed);
    if r >= 0 {
        resync(dev);
    }
    r
}

/// Tag a new state block, so its Apply comes to us.
fn block(sb: Ptr) {
    if !sb.is_null()
        && !unsafe { BLOCKS.adopt(sb, VTABLE, &[(SB_APPLY, sb_apply as *const () as usize)]) }
    {
        log("state block of another class: Apply not seen");
    }
}

unsafe extern "system" fn create_state_block(dev: Ptr, ty: u32, out: *mut Ptr) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, u32, *mut Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_CREATE_STATE_BLOCK)) };
    let r = unsafe { f(dev, ty, out) };
    if r >= 0 && !out.is_null() {
        block(unsafe { *out });
    }
    r
}

unsafe extern "system" fn begin_state_block(dev: Ptr) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_BEGIN_STATE_BLOCK)) };
    let r = unsafe { f(dev) };
    if r >= 0 {
        RECORDING.store(true, Relaxed);
    }
    r
}

unsafe extern "system" fn end_state_block(dev: Ptr, out: *mut Ptr) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, *mut Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_END_STATE_BLOCK)) };
    let r = unsafe { f(dev, out) };
    RECORDING.store(false, Relaxed);
    if r >= 0 && !out.is_null() {
        block(unsafe { *out });
    }
    r
}

unsafe extern "system" fn sb_apply(sb: Ptr) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr) -> HRESULT =
        unsafe { std::mem::transmute(BLOCKS.orig(SB_APPLY)) };
    let r = unsafe { f(sb) };
    let dev = DEV.load(Relaxed);
    if r >= 0 && dev != 0 {
        resync(dev as Ptr);
    }
    r
}
