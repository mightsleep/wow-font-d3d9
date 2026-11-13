//! At every upload of a glyph cache (and at texture::refresh), the cells
//! that check out go into v2's textures beside it (v2.rs); the rest draw
//! as the client drew them. A cell checks out if its texture's owner still
//! leads to this texture (the client recycles them between fonts) and its
//! alpha holds its glyph's bitmap (95 % of the strong pixels).

use std::sync::Mutex;

use crate::cache::{self, FxMap, fx_map};
use crate::glyph::Placement;
use crate::{fontfile, fonts, log, mem, settings};

/// Drop every remembered encoding (a setting it depends on changed).
pub fn forget_encodings() {
    crate::v2::forget_encodings();
}

/// The texels of a glyph cache the client changed since the payload last
/// went over it, a bit each (texture.rs keeps them per cache).
pub struct Dirty(Box<[u64; 1024]>);

impl Dirty {
    pub fn none() -> Self {
        Dirty(Box::new([0; 1024]))
    }

    pub fn all() -> Self {
        Dirty(Box::new([!0; 1024]))
    }

    pub fn set(&mut self, x: u32, y: u32) {
        self.0[(y * 4 + x / 64) as usize] |= 1 << (x % 64);
    }

    /// Whether any texel in columns x0..=x1 of rows y0..y1 changed.
    fn any(&self, x0: u32, x1: u32, y0: u32, y1: u32) -> bool {
        let x1 = x1.min(255);
        (y0..y1.min(256)).any(|y| {
            (x0 / 64..=x1 / 64).any(|k| {
                let lo = if k == x0 / 64 { x0 % 64 } else { 0 };
                let hi = if k == x1 / 64 { x1 % 64 } else { 63 };
                self.0[(y * 4 + k) as usize] & (!0u64 >> (63 - hi)) & (!0u64 << lo) != 0
            })
        })
    }
}

/// Per glyph cache, the font and per cell (code, row, start, end) whether
/// it held its glyph's bitmap at the last pass (None: no bitmap to check).
/// A cell none of whose texels changed since keeps its verdict without its
/// bitmap read from the client again.
struct Checked {
    base: u32,
    cells: FxMap<(u32, u32, u32, u32), Option<bool>>,
}

static CHECKED: Mutex<FxMap<usize, Checked>> = Mutex::new(fx_map());

/// Where the client pastes a font's bitmaps in its cells, (dx, dy), by
/// (font file fingerprint, pixel size, flags, cell height): the same for
/// every font object of one file (the client makes several) and from
/// session to session (kept in the bundle).
static OFFSETS: Mutex<FxMap<(u64, u32, u32, u32), (u32, u32)>> = Mutex::new(fx_map());

/// The client's texels of a glyph cache, 256 x 256 ARGB4444.
struct Texels<'a>(&'a [u16]);

impl Texels<'_> {
    fn get(&self, x: u32, y: u32) -> u16 {
        self.0[(y * 256 + x) as usize]
    }
    fn opaque(&self, x: u32, y: u32) -> bool {
        x < 256 && y < 256 && self.get(x, y) >> 12 >= 8
    }
    /// An outlined font keeps the fill in RGB and the outline in alpha.
    fn filled(&self, x: u32, y: u32) -> bool {
        x < 256 && y < 256 && (self.get(x, y) >> 8) & 15 >= 8
    }
}

/// The client's bitmap of a cell as 8-bit coverage into `out`, through
/// `raw` (both kept from cell to cell); false where there is none.
fn bitmap(g: &mem::Cell, raw: &mut Vec<u8>, out: &mut Vec<u8>) -> bool {
    let (w, h) = (g.glyph_width, g.glyph_height);
    if g.data == 0 || w == 0 || h == 0 || w > 64 || h > 64 || g.pitch < w || g.pitch > 256 {
        return false;
    }
    raw.resize((g.pitch * h) as usize, 0);
    if !mem::read_into(g.data, raw) {
        return false;
    }
    let (w, p) = (w as usize, g.pitch as usize);
    out.clear();
    for y in 0..h as usize {
        out.extend_from_slice(&raw[y * p..y * p + w]);
    }
    true
}

/// (strong pixels, those found in the texture) for a cell pasted at (dx, dy).
fn score(
    t: &Texels,
    g: &mem::Cell,
    b: &[u8],
    ch: u32,
    (dx, dy): (u32, u32),
    fill: bool,
) -> (u32, u32) {
    let (x0, y0) = (g.start + dx, g.row * ch + g.y_start + dy);
    let (mut strong, mut hit) = (0, 0);
    for y in 0..g.glyph_height {
        for x in 0..g.glyph_width {
            if b[(y * g.glyph_width + x) as usize] >= 128 {
                strong += 1;
                hit += if fill {
                    t.filled(x0 + x, y0 + y)
                } else {
                    t.opaque(x0 + x, y0 + y)
                } as u32;
            }
        }
    }
    (strong, hit)
}

/// The best (dx, dy) over the first glyphs, taken when it matches nearly
/// everywhere and clearly beats the runner-up (other fonts' residue in the
/// client's shared paste buffer costs a few percent). An outlined font's
/// alpha is the glyph dilated and would match a pixel off as well, so its
/// fill is matched against RGB, where it is exact.
fn find_placement(t: &Texels, font: &mem::Font, cells: &[mem::Cell]) -> Option<(u32, u32)> {
    let fill = font.flags & 0x9 != 0;
    let mut raw = Vec::new();
    let sample: Vec<(&mem::Cell, Vec<u8>)> = cells
        .iter()
        .filter_map(|g| {
            let mut b = Vec::new();
            bitmap(g, &mut raw, &mut b).then_some((g, b))
        })
        .take(24)
        .collect();
    let mut grid: Vec<(u32, u32, (u32, u32))> = (0..16)
        .map(|k| {
            let o = (k / 4, k % 4);
            let (s, h) = sample.iter().fold((0, 0), |a, (g, b)| {
                let (s, h) = score(t, g, b, font.cell_height, o, fill);
                (a.0 + s, a.1 + h)
            });
            (h, s, o)
        })
        .collect();
    let line: Vec<String> = grid
        .iter()
        .map(|&(h, s, (dx, dy))| format!("{},{}={}/{}", dx, dy, h, s))
        .collect();
    log(&format!(
        "{} px={} flags={:#x}: placement by {}: {}",
        font.name,
        font.pixel_size,
        font.flags,
        if fill { "fill" } else { "alpha" },
        line.join(" ")
    ));
    // Most matches first; on a tie the smaller offset.
    grid.sort_by_key(|&(h, _, (dx, dy))| std::cmp::Reverse((h, std::cmp::Reverse(dx + dy))));
    let (h, s, o) = grid[0];
    let second = grid[1].0;
    (s > 0 && h * 100 >= s * 95 && (h - second) * 100 >= s * 5).then_some(o)
}

/// Put the cells of glyph cache `tex` that check out against `texels`
/// (what the client uploaded) into v2's textures, `dirty` the texels the
/// client changed since the last time: how many cells went in, and a
/// summary for the log.
pub fn write(tex: usize, texels: &[u16], dirty: &Dirty) -> (u32, String) {
    let mut written = 0;
    let summary = write_cells(tex, texels, dirty, &mut written);
    (written, summary)
}

fn write_cells(tex: usize, texels: &[u16], dirty: &Dirty, cells: &mut u32) -> String {
    let trace = crate::trace::span(crate::trace::Kind::Font);
    // Verdicts hold only for texels seen since: taken out here and put back
    // only by a pass that gets through every cell.
    let verdicts = CHECKED.lock().unwrap().remove(&tex);
    let Some((header, cache_index)) = fonts::owners()
        .lock()
        .unwrap()
        .get(&tex)
        .map(|o| (o.header, o.cache))
    else {
        return "no font yet".into();
    };
    // An owner that no longer checks out is forgotten; the next upload
    // names the new one.
    let Some(font) = mem::reparse(header) else {
        fonts::lost(tex);
        return "font gone".into();
    };
    let Some(cache) = font.caches.iter().find(|c| c.index == cache_index) else {
        fonts::lost(tex);
        return format!("{}: cache gone", font.name);
    };
    if fonts::texture_of(cache.texture) != Some(tex as u32) {
        fonts::lost(tex);
        return format!(
            "{} px={}: texture went to another font",
            font.name, font.pixel_size
        );
    }
    if !crate::v2::eligible(font.flags) {
        return format!(
            "{} px={} flags={:#x}: left as bitmap",
            font.name, font.pixel_size, font.flags
        );
    }
    let Some(data) = fontfile::load(&font.name) else {
        return format!("{}: no font file", font.name);
    };
    let Some(fp) = cache::fingerprint(&data) else {
        return format!("{}: no sfnt", font.name);
    };
    let t = Texels(texels);
    let ch = font.cell_height;
    let bundle = cache::Params {
        font: fp,
        pixel_size: font.pixel_size,
        flags: font.flags,
        gamma: settings::GAMMA.get(),
        radius: crate::v2::radius_of(font.flags),
    };

    let key = (fp, font.pixel_size, font.flags, ch);
    let known = OFFSETS.lock().unwrap().get(&key).copied();
    let Some((dx, dy)) = known.or_else(|| cache::placement(&bundle)).or_else(|| {
        let o = find_placement(&t, &font, &cache.cells)?;
        cache::put_placement(&bundle, o);
        Some(o)
    }) else {
        return format!(
            "{} px={}: bitmaps not found in the texture",
            font.name, font.pixel_size
        );
    };
    OFFSETS.lock().unwrap().insert(key, (dx, dy));

    drop(trace);
    let trace = crate::trace::span(crate::trace::Kind::Clear);
    if !crate::v2::begin(tex, font.base, ch, font.pixel_size, font.flags, fp, &data) {
        return format!("{} px={}: no v2 textures", font.name, font.pixel_size);
    }
    drop(trace);
    let trace = crate::trace::span(crate::trace::Kind::Cells);
    let mut verdicts = verdicts
        .filter(|v| v.base == font.base)
        .unwrap_or_else(|| Checked {
            base: font.base,
            cells: fx_map(),
        });
    let mut seen = fx_map();
    let (mut raw, mut b) = (Vec::new(), Vec::new());
    let (mut written, mut foreign, mut empty) = (0, 0, 0);
    for g in &cache.cells {
        let id = (g.code, g.row, g.start, g.end);
        let y0 = g.row * ch;
        let verdict = match verdicts.cells.get(&id) {
            Some(&v) if !dirty.any(g.start, g.end, y0, y0 + ch) => v,
            _ => bitmap(g, &mut raw, &mut b).then(|| {
                let (s, h) = score(&t, g, &b, ch, (dx, dy), false);
                s > 0 && h * 100 >= s * 95
            }),
        };
        seen.insert(id, verdict);
        match verdict {
            None => continue,
            Some(false) => {
                foreign += 1;
                continue;
            }
            Some(true) => {}
        }
        // The cell table is read while the client may be rewriting it: a
        // cell outside the texture (or end before start, which would make
        // w wrap to 4 billion and the encoder's allocation abort the game)
        // is skipped.
        if g.end < g.start || g.end >= 256 || g.end - g.start >= 128 || y0 + ch > 256 {
            continue;
        }
        let w = g.end - g.start + 1;
        let at = Placement {
            pixel_size: font.pixel_size,
            dx,
            top: g.y_start + dy,
        };
        if crate::v2::cell(tex, fp, &data, g.code, &at, g.start, g.row, w, &bundle) {
            written += 1;
        } else {
            empty += 1;
        }
    }
    drop(trace);
    {
        let _trace = crate::trace::span(crate::trace::Kind::Commit);
        crate::v2::commit(tex);
    }
    *cells = written;
    if written > 0 {
        FRESH.lock().unwrap().current.insert(tex, font.base);
    }
    verdicts.cells = seen;
    CHECKED.lock().unwrap().insert(tex, verdicts);
    format!(
        "{} px={} cell={} flags={:#x} at dx={} dy={}: {} cells in v2, {} not in the texture, {} without outline",
        font.name, font.pixel_size, ch, font.flags, dx, dy, written, foreign, empty
    )
}

/// Per glyph cache, the font header whose cells the texture last got and the
/// last one a refresh was tried for.
struct Fresh {
    current: FxMap<usize, u32>,
    tried: FxMap<usize, u32>,
}

static FRESH: Mutex<Fresh> = Mutex::new(Fresh {
    current: fx_map(),
    tried: fx_map(),
});

/// Whether `tex` lacks the cells of font `header` and no refresh was tried
/// for it: the client uploaded it before the font was known, and will not
/// again until a new glyph comes.
pub fn needs_refresh(tex: usize, header: u32) -> bool {
    let f = FRESH.lock().unwrap();
    f.current.get(&tex) != Some(&header) && f.tried.get(&tex) != Some(&header)
}

pub fn tried(tex: usize, header: u32) {
    FRESH.lock().unwrap().tried.insert(tex, header);
}
