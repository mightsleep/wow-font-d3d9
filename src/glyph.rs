//! Glyph outlines from a font file, placed in the client's cell. Pure
//! (ttf-parser only), so the bench builds cells the same way.

use ttf_parser::{Face, OutlineBuilder};

pub type P = [f32; 2];

/// A quadratic: start, control, end. A line has its control halfway.
pub type Quad = [P; 3];

pub struct Glyph {
    pub contours: Vec<Vec<Quad>>,
    /// x_min, y_min, x_max, y_max in font units.
    pub bbox: [f32; 4],
    pub upem: f32,
}

/// Where the client pasted a glyph's bitmap in its cell.
pub struct Placement {
    pub pixel_size: u32,
    /// Left edge of the bitmap in the cell (the outline's padding).
    pub dx: u32,
    /// Top edge of the bitmap in the cell: m_yStart plus the padding.
    pub top: u32,
}

fn mid(a: P, b: P) -> P {
    [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]
}

#[derive(Default)]
struct Builder {
    contours: Vec<Vec<Quad>>,
    cur: Vec<Quad>,
    start: P,
    pen: P,
}

impl Builder {
    fn push(&mut self, a: P, b: P, c: P) {
        if a != c || a != b {
            self.cur.push([a, b, c]);
        }
        self.pen = c;
    }
    fn finish(&mut self) {
        if self.pen != self.start {
            let (a, b) = (self.pen, self.start);
            self.push(a, mid(a, b), b);
        }
        if !self.cur.is_empty() {
            self.contours.push(std::mem::take(&mut self.cur));
        }
    }
}

impl OutlineBuilder for Builder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.finish();
        self.start = [x, y];
        self.pen = [x, y];
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let a = self.pen;
        self.push(a, mid(a, [x, y]), [x, y]);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let a = self.pen;
        self.push(a, [x1, y1], [x, y]);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        // A CFF font's cubic, as one quadratic (the midpoint approximation).
        let a = self.pen;
        let c = [
            (3.0 * (x1 + x2) - a[0] - x) * 0.25,
            (3.0 * (y1 + y2) - a[1] - y) * 0.25,
        ];
        self.push(a, c, [x, y]);
    }
    fn close(&mut self) {
        self.finish();
    }
}

pub fn outline(data: &[u8], code: u32) -> Option<Glyph> {
    let face = Face::parse(data, 0).ok()?;
    let gid = face.glyph_index(char::from_u32(code)?)?;
    let mut b = Builder::default();
    let bb = face.outline_glyph(gid, &mut b)?;
    b.finish();
    Some(Glyph {
        contours: b.contours,
        bbox: [
            bb.x_min as f32,
            bb.y_min as f32,
            bb.x_max as f32,
            bb.y_max as f32,
        ],
        upem: face.units_per_em() as f32,
    })
}

impl Glyph {
    /// The contours in cell texels, y down, where the client's bitmap is:
    /// its left and top edges are the outline's box rounded out to pixels.
    pub fn placed(&self, at: &Placement) -> Vec<Vec<Quad>> {
        let s = at.pixel_size as f32 / self.upem;
        let (left, top) = ((self.bbox[0] * s).floor(), (self.bbox[3] * s).ceil());
        let map = |p: P| {
            [
                at.dx as f32 + (p[0] * s - left),
                at.top as f32 + (top - p[1] * s),
            ]
        };
        self.contours
            .iter()
            .map(|c| c.iter().map(|q| q.map(map)).collect())
            .collect()
    }
}

pub fn point(q: &Quad, t: f32) -> P {
    let u = 1.0 - t;
    let [a, b, c] = *q;
    [
        u * u * a[0] + 2.0 * u * t * b[0] + t * t * c[0],
        u * u * a[1] + 2.0 * u * t * b[1] + t * t * c[1],
    ]
}
