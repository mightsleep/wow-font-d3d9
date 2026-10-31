//! wow-font-d3d9.ini next to Wow.exe (`key = number` a line, # comments),
//! read when the device is made and again whenever it changes.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use crate::{beside_exe, log, payload};
pub struct Knob {
    key: &'static str,
    default: f32,
    value: AtomicU32,
}

impl Knob {
    const fn new(key: &'static str, default: f32) -> Knob {
        Knob {
            key,
            default,
            value: AtomicU32::new(default.to_bits()),
        }
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.value.load(Relaxed))
    }
}

pub static GAMMA: Knob = Knob::new("gamma", 1.5);
/// Pixels beyond the client's outline: its box-filtered edge reads thinner
/// than a hard one of the same radius (0.25 by eye).
pub static OUTLINE: Knob = Knob::new("outline", 0.25);
/// Format v2 (the curve decoder, v2.rs): 1 on, 0 the client's bitmaps
/// for every font (as without the proxy).
pub static V2: Knob = Knob::new("v2", 1.0);
/// v2's texel words (atlas::texel_words) for magnified text: 1 on, 0 the
/// bands for every pixel, 2 on and the words' path in colour (magenta the
/// runs, green none needed). Read at each draw, so no cell is encoded again.
pub static WORDS: Knob = Knob::new("words", 1.0);

const KNOBS: [&Knob; 4] = [&GAMMA, &OUTLINE, &V2, &WORDS];
/// Those the cells' encoding depends on (v2 bakes the gamma and the
/// outline into its cover bytes).
const ENCODING: [&Knob; 3] = [&GAMMA, &V2, &OUTLINE];

/// Bumped whenever every cache must be written again.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(Relaxed)
}

/// Read the ini; true if anything the encoding depends on changed.
fn load_ini(path: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let before: Vec<f32> = ENCODING.iter().map(|k| k.get()).collect();
    // The file is the whole setting: a line taken out is the default again.
    // Each knob is stored once, so a draw meanwhile sees the old value or
    // the new, never the default in between.
    let mut values: Vec<f32> = KNOBS.iter().map(|k| k.default).collect();
    let mut set = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let Some((key, v)) = line.split_once('=') else {
            continue;
        };
        let (Some(i), Ok(v)) = (
            KNOBS.iter().position(|k| k.key == key.trim()),
            v.trim().parse::<f32>(),
        ) else {
            continue;
        };
        values[i] = v;
        set.push(format!("{} = {}", KNOBS[i].key, v));
    }
    for (knob, v) in KNOBS.iter().zip(values) {
        knob.value.store(v.to_bits(), Relaxed);
    }
    log(&format!(
        "settings: {}",
        if set.is_empty() {
            "nothing read".into()
        } else {
            set.join(", ")
        }
    ));
    ENCODING.iter().map(|k| k.get()).ne(before)
}

/// Read the ini now, then look for changes every 250 ms from a thread of
/// its own, which also lets fontfile close the MPQ chain: a stat costs
/// about 0.5 ms under Wine, too much to do in a draw.
pub fn watch() {
    let ini = beside_exe("wow-font-d3d9.ini");
    let mut seen = None;
    let mut check = move || {
        let m = std::fs::metadata(&ini).and_then(|m| m.modified()).ok();
        if m.is_some() && m != seen {
            seen = m;
            if load_ini(&ini) {
                payload::forget_encodings();
                GENERATION.fetch_add(1, Relaxed);
            }
        }
    };
    check();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(250));
        crate::fontfile::tidy();
        check();
    });
}
