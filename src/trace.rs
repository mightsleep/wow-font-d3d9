//! Tracing, with WOW_FONT_D3D9_TRACE=1 (read once, when the device is
//! made): where the time goes, ours on the CPU and the text's on the GPU,
//! one line to the log every 5 s.
//!
//! - CPU: scopes around our own work (span()). They nest: bind holds
//!   refresh, refresh holds payload.
//! - GPU: timestamp queries at each Present and wherever a glyph cache
//!   goes on or off stage 0 (phi calls text()), so a run of text draws sits
//!   between two of them. Results come back a few frames later.
//!
//! Draws overlap in the GPU's pipeline, so a run's time is roughly the
//! text's, not exactly; the frame's includes idle time when the CPU is the
//! bottleneck.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::{dev_orig, log, Ptr, HRESULT};

const DEV_PRESENT: usize = 17;
const DEV_CREATE_QUERY: usize = 118;

const Q_ISSUE: usize = 6;
const Q_GET_DATA: usize = 7;
const ISSUE_END: u32 = 1;
const ISSUE_BEGIN: u32 = 2;

const QT_TIMESTAMP: u32 = 10;
const QT_TIMESTAMP_DISJOINT: u32 = 11;
const QT_TIMESTAMP_FREQ: u32 = 12;

/// Text runs timed per frame; past them the frame's text time falls short.
const MAX_RUNS: usize = 256;
/// Frames whose queries may wait for the GPU at once.
const IN_FLIGHT: usize = 6;
const WINDOW_S: f64 = 5.0;

#[derive(Clone, Copy)]
pub enum Kind {
    Bind,
    Refresh,
    Payload,
    Identify,
}
const NAMES: [&str; 4] = ["bind", "refresh", "payload", "identify"];

static ON: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool {
    ON.load(Relaxed)
}

/// Read the switch; true if tracing.
pub fn init() -> bool {
    let on = std::env::var("WOW_FONT_D3D9_TRACE").is_ok_and(|v| v == "1");
    ON.store(on, Relaxed);
    on
}

/// A CPU scope: its time goes to `kind` when it drops.
pub struct Span(Option<(Kind, Instant)>);

pub fn span(kind: Kind) -> Span {
    Span(enabled().then(|| (kind, Instant::now())))
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((k, t)) = self.0 {
            let ns = t.elapsed().as_nanos() as u64;
            let a = &mut state().lock().unwrap().win.cpu[k as usize];
            a.n += 1;
            a.ns += ns;
            a.max = a.max.max(ns);
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Acc {
    n: u64,
    ns: u64,
    max: u64,
}

struct Query(usize);

impl Query {
    fn new(dev: Ptr, ty: u32) -> Option<Query> {
        let f: unsafe extern "system" fn(Ptr, u32, *mut usize) -> HRESULT =
            unsafe { std::mem::transmute(dev_orig(DEV_CREATE_QUERY)) };
        let mut q = 0usize;
        (unsafe { f(dev, ty, &mut q) } >= 0 && q != 0).then_some(Query(q))
    }

    fn method(&self, i: usize) -> usize {
        unsafe { *(*(self.0 as *const *const usize)).add(i) }
    }

    fn issue(&self, flags: u32) {
        let f: unsafe extern "system" fn(usize, u32) -> HRESULT =
            unsafe { std::mem::transmute(self.method(Q_ISSUE)) };
        unsafe { f(self.0, flags) };
    }

    /// Ok(None) while the GPU has not got there; Err for a lost device.
    fn get<T: Default>(&self) -> Result<Option<T>, HRESULT> {
        let f: unsafe extern "system" fn(usize, *mut c_void, u32, u32) -> HRESULT =
            unsafe { std::mem::transmute(self.method(Q_GET_DATA)) };
        let mut v = T::default();
        match unsafe {
            f(
                self.0,
                &mut v as *mut T as *mut c_void,
                size_of::<T>() as u32,
                0,
            )
        } {
            0 => Ok(Some(v)),
            1 => Ok(None),
            r => Err(r),
        }
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        let f: unsafe extern "system" fn(usize) -> u32 =
            unsafe { std::mem::transmute(self.method(2)) };
        unsafe { f(self.0) };
    }
}

/// One frame's queries; reused once read.
struct Frame {
    disjoint: Query,
    freq: Query,
    begin: Query,
    end: Query,
    /// Text runs: a timestamp at each end.
    runs: Vec<[Query; 2]>,
    used: usize,
    open: bool,
}

/// A frame as the GPU timed it; None for a disjoint one.
type Gpu = Option<(f64, f64, usize)>;

impl Frame {
    fn new(dev: Ptr) -> Option<Frame> {
        Some(Frame {
            disjoint: Query::new(dev, QT_TIMESTAMP_DISJOINT)?,
            freq: Query::new(dev, QT_TIMESTAMP_FREQ)?,
            begin: Query::new(dev, QT_TIMESTAMP)?,
            end: Query::new(dev, QT_TIMESTAMP)?,
            runs: Vec::new(),
            used: 0,
            open: false,
        })
    }

    fn start(&mut self) {
        self.used = 0;
        self.open = false;
        self.disjoint.issue(ISSUE_BEGIN);
        self.begin.issue(ISSUE_END);
    }

    fn text(&mut self, dev: Ptr, on: bool) {
        if on && !self.open {
            if self.used == self.runs.len() {
                let q = (self.used < MAX_RUNS).then(|| {
                    Some([
                        Query::new(dev, QT_TIMESTAMP)?,
                        Query::new(dev, QT_TIMESTAMP)?,
                    ])
                });
                match q.flatten() {
                    Some(q) => self.runs.push(q),
                    None => return,
                }
            }
            self.runs[self.used][0].issue(ISSUE_END);
            self.used += 1;
            self.open = true;
        } else if !on && self.open {
            self.runs[self.used - 1][1].issue(ISSUE_END);
            self.open = false;
        }
    }

    fn finish(&mut self, dev: Ptr) {
        self.text(dev, false);
        self.end.issue(ISSUE_END);
        self.disjoint.issue(ISSUE_END);
        self.freq.issue(ISSUE_END);
    }

    /// Frame ms, text ms, runs.
    fn read(&self) -> Result<Option<Gpu>, HRESULT> {
        let (Some(freq), Some(disjoint), Some(b), Some(e)) = (
            self.freq.get::<u64>()?,
            self.disjoint.get::<i32>()?,
            self.begin.get::<u64>()?,
            self.end.get::<u64>()?,
        ) else {
            return Ok(None);
        };
        let ms = |b: u64, e: u64| e.saturating_sub(b) as f64 * 1e3 / freq.max(1) as f64;
        let mut text = 0.0;
        for [qb, qe] in &self.runs[..self.used] {
            let (Some(b), Some(e)) = (qb.get::<u64>()?, qe.get::<u64>()?) else {
                return Ok(None);
            };
            text += ms(b, e);
        }
        Ok(Some((disjoint == 0).then_some((ms(b, e), text, self.used))))
    }
}

#[derive(Default)]
struct Window {
    frames: u64,
    timed: u64,
    disjoint: u64,
    gpu_ms: f64,
    text_ms: f64,
    runs: u64,
    cpu: [Acc; 4],
}

struct State {
    dev: usize,
    /// Query creation failed: CPU side only.
    no_gpu: bool,
    cur: Option<Frame>,
    free: Vec<Frame>,
    pending: VecDeque<Frame>,
    /// A glyph cache is on stage 0.
    text: bool,
    win: Window,
    start: Instant,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            dev: 0,
            no_gpu: false,
            cur: None,
            free: Vec::new(),
            pending: VecDeque::new(),
            text: false,
            win: Window::default(),
            start: Instant::now(),
        })
    })
}

impl State {
    fn end_frame(&mut self) {
        let dev = self.dev as Ptr;
        if let Some(mut f) = self.cur.take() {
            f.finish(dev);
            self.pending.push_back(f);
        }
    }

    fn after_present(&mut self) {
        self.win.frames += 1;
        self.drain();
        if self.start.elapsed().as_secs_f64() >= WINDOW_S {
            self.flush();
        }
        let dev = self.dev as Ptr;
        if self.no_gpu || self.pending.len() >= IN_FLIGHT {
            return;
        }
        match self.free.pop().or_else(|| Frame::new(dev)) {
            Some(mut f) => {
                f.start();
                f.text(dev, self.text);
                self.cur = Some(f);
            }
            None => {
                self.no_gpu = true;
                log("trace: the device has no timestamp queries, CPU side only");
            }
        }
    }

    /// Book the frames whose queries are in, in order.
    fn drain(&mut self) {
        while let Some(f) = self.pending.front() {
            let w = &mut self.win;
            match f.read() {
                Ok(None) => break,
                Ok(Some(None)) => w.disjoint += 1,
                Ok(Some(Some((frame, text, runs)))) => {
                    w.timed += 1;
                    w.gpu_ms += frame;
                    w.text_ms += text;
                    w.runs += runs as u64;
                }
                Err(r) => {
                    log(&format!(
                        "trace: query read failed {:#x}, its queries dropped",
                        r
                    ));
                    self.pending.pop_front();
                    continue;
                }
            }
            let f = self.pending.pop_front().unwrap();
            self.free.push(f);
        }
    }

    fn flush(&mut self) {
        let w = std::mem::take(&mut self.win);
        let secs = self.start.elapsed().as_secs_f64();
        self.start = Instant::now();
        let n = w.frames.max(1) as f64;
        let t = w.timed.max(1) as f64;
        let cpu = NAMES
            .iter()
            .zip(&w.cpu)
            .map(|(name, a)| {
                format!(
                    "{} {:.1} us ({:.1}x, max {:.0} us)",
                    name,
                    a.ns as f64 / 1e3 / n,
                    a.n as f64 / n,
                    a.max as f64 / 1e3
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        log(&format!(
            "trace {:.1} s, {} frames: gpu ({} timed, {} disjoint) frame {:.3} ms, text {:.3} ms ({:.1} %) in {:.1} runs; ours per frame: {}",
            secs,
            w.frames,
            w.timed,
            w.disjoint,
            w.gpu_ms / t,
            w.text_ms / t,
            100.0 * w.text_ms / w.gpu_ms.max(1e-9),
            w.runs as f64 / t,
            cpu
        ));
    }
}

/// A glyph cache went on (or off) stage 0.
pub fn text(on: bool) {
    if !enabled() {
        return;
    }
    let mut s = state().lock().unwrap();
    s.text = on;
    let dev = s.dev as Ptr;
    if let Some(f) = s.cur.as_mut() {
        f.text(dev, on);
    }
}

/// Before a Reset: the queries go (the device wants nothing of its own
/// left), the GPU side starts over at the next Present.
pub fn reset() {
    if !enabled() {
        return;
    }
    let mut s = state().lock().unwrap();
    s.cur = None;
    s.free.clear();
    s.pending.clear();
}

pub fn device(dev: Ptr) {
    state().lock().unwrap().dev = dev as usize;
    log("trace: on");
}

pub fn hooks() -> [(usize, usize); 1] {
    [(DEV_PRESENT, present as *const () as usize)]
}

unsafe extern "system" fn present(
    dev: Ptr,
    src: *const c_void,
    dst: *const c_void,
    wnd: Ptr,
    dirty: *const c_void,
) -> HRESULT {
    let f: unsafe extern "system" fn(
        Ptr,
        *const c_void,
        *const c_void,
        Ptr,
        *const c_void,
    ) -> HRESULT = unsafe { std::mem::transmute(dev_orig(DEV_PRESENT)) };
    state().lock().unwrap().end_frame();
    let r = unsafe { f(dev, src, dst, wnd, dirty) };
    state().lock().unwrap().after_present();
    r
}
