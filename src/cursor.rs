//! Under Wine the client's hardware cursor blinks while the mouse moves.
//! At every WM_SETCURSOR, so at every move, the client hides the Windows
//! cursor and shows Direct3D's (the DirectX SDK's idiom), and DXVK shows
//! Direct3D's by setting the Windows cursor again. On Windows nobody sees
//! the two; under Wine each goes to the X server on its own (a trace has
//! SetCursor(NULL) and SetCursor(cursor) 0.5 ms apart, 1883 times in 84
//! s) and a frame can fall between them.
//!
//! With WOW_FONT_D3D9_CURSOR=1 a window procedure of ours stands before
//! the client's and answers WM_SETCURSOR itself while Direct3D's cursor
//! shows: the cursor is the one DXVK set, nothing is hidden, the client
//! is not asked. Which cursor that is comes from the device: after
//! SetCursorProperties and ShowCursor go through, the thread's cursor is
//! DXVK's. A cursor DXVK draws itself (not 32x32) or one that is hidden
//! leaves the thread's cursor null, and the client answers as before.
//!
//! This is the one hook outside Direct3D 9, and it is off unless asked
//! for. No code is patched: a window's procedure is a field Windows
//! lets anyone in the process set.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

use crate::{HRESULT, Ptr, dev_orig, log};

#[link(name = "user32")]
unsafe extern "system" {
    fn GetCursor() -> Ptr;
    fn SetCursor(cursor: Ptr) -> Ptr;
    fn IsWindowUnicode(wnd: Ptr) -> i32;
    fn SetWindowLongA(wnd: Ptr, index: i32, value: i32) -> i32;
    fn SetWindowLongW(wnd: Ptr, index: i32, value: i32) -> i32;
    fn CallWindowProcA(prev: usize, wnd: Ptr, msg: u32, w: usize, l: isize) -> isize;
    fn CallWindowProcW(prev: usize, wnd: Ptr, msg: u32, w: usize, l: isize) -> isize;
}

const DEV_SET_CURSOR_PROPERTIES: usize = 10;
const DEV_SHOW_CURSOR: usize = 12;
const GWL_WNDPROC: i32 = -4;
const WM_SETCURSOR: u32 = 0x20;
const HTCLIENT: u32 = 1;

/// The window in front of whose procedure ours stands, and that one.
static WINDOW: AtomicUsize = AtomicUsize::new(0);
static CLIENTS: AtomicUsize = AtomicUsize::new(0);
static UNICODE: AtomicBool = AtomicBool::new(false);
/// Whether Direct3D's cursor is shown, and the Windows cursor DXVK set
/// for it (0: none, the client answers WM_SETCURSOR).
static VISIBLE: AtomicBool = AtomicBool::new(false);
static SHOWN: AtomicUsize = AtomicUsize::new(0);
static KEPT: AtomicUsize = AtomicUsize::new(0);

pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var("WOW_FONT_D3D9_CURSOR").is_ok_and(|v| v == "1"))
}

pub fn hooks() -> Vec<(usize, usize)> {
    if !enabled() {
        return Vec::new();
    }
    vec![
        (
            DEV_SET_CURSOR_PROPERTIES,
            set_cursor_properties as *const () as usize,
        ),
        (DEV_SHOW_CURSOR, show_cursor as *const () as usize),
    ]
}

/// The thread's cursor after the device took a cursor call.
fn note() {
    let c = if VISIBLE.load(Relaxed) {
        unsafe { GetCursor() as usize }
    } else {
        0
    };
    SHOWN.store(c, Relaxed);
}

unsafe extern "system" fn set_cursor_properties(dev: Ptr, x: u32, y: u32, bitmap: Ptr) -> HRESULT {
    let f: unsafe extern "system" fn(Ptr, u32, u32, Ptr) -> HRESULT =
        unsafe { std::mem::transmute(dev_orig(DEV_SET_CURSOR_PROPERTIES)) };
    let r = unsafe { f(dev, x, y, bitmap) };
    note();
    r
}

unsafe extern "system" fn show_cursor(dev: Ptr, show: i32) -> i32 {
    let f: unsafe extern "system" fn(Ptr, i32) -> i32 =
        unsafe { std::mem::transmute(dev_orig(DEV_SHOW_CURSOR)) };
    let r = unsafe { f(dev, show) };
    VISIBLE.store(show != 0, Relaxed);
    note();
    r
}

unsafe extern "system" fn window_proc(wnd: Ptr, msg: u32, w: usize, l: isize) -> isize {
    if msg == WM_SETCURSOR && (l as u32 & 0xffff) == HTCLIENT {
        let c = SHOWN.load(Relaxed);
        if c != 0 {
            // Another window's cursor may be up (the pointer came back
            // in): then ours once, and not again while it stays.
            if unsafe { GetCursor() } as usize != c {
                unsafe { SetCursor(c as Ptr) };
            }
            if KEPT.fetch_add(1, Relaxed) == 0 {
                log("cursor: the first WM_SETCURSOR answered here, the client not asked");
            }
            return 1;
        }
    }
    let prev = CLIENTS.load(Relaxed);
    if UNICODE.load(Relaxed) {
        unsafe { CallWindowProcW(prev, wnd, msg, w, l) }
    } else {
        unsafe { CallWindowProcA(prev, wnd, msg, w, l) }
    }
}

/// Stand before the procedure of the device's window: the one in its
/// presentation parameters (hDeviceWindow, the eighth field), else the
/// focus window. Once a window.
pub unsafe fn device(focus: Ptr, params: Ptr) {
    if !enabled() {
        return;
    }
    let mut wnd = if params.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe { *(params as *const Ptr).add(7) }
    };
    if wnd.is_null() {
        wnd = focus;
    }
    if wnd.is_null() || WINDOW.load(Relaxed) == wnd as usize {
        return;
    }
    if WINDOW.load(Relaxed) != 0 {
        log("cursor: a second window, left alone");
        return;
    }
    let unicode = unsafe { IsWindowUnicode(wnd) } != 0;
    let ours = window_proc as *const () as usize as i32;
    // The client's procedure has to be known before ours can be called.
    UNICODE.store(unicode, Relaxed);
    let prev = unsafe {
        if unicode {
            SetWindowLongW(wnd, GWL_WNDPROC, ours)
        } else {
            SetWindowLongA(wnd, GWL_WNDPROC, ours)
        }
    } as u32 as usize;
    if prev == 0 {
        log("cursor: the window's procedure could not be set");
        return;
    }
    CLIENTS.store(prev, Relaxed);
    WINDOW.store(wnd as usize, Relaxed);
    log(&format!(
        "cursor: WM_SETCURSOR of window {:#x} ({}) answered here while Direct3D's cursor shows",
        wnd as usize,
        if unicode { "Unicode" } else { "ANSI" }
    ));
}
