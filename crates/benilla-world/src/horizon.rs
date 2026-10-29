//! The horizon occlusion cull: what lies behind nearer terrain is not drawn.
//!
//! The reference keeps 320 floats at `0xc7b750`, one per screen column, each the highest
//! view-space `y/z` an occluder reached in that column. Once a frame `0x680bc0` builds the level
//! camera `0xc7b700` the buffer projects through, `0x685fe0` fills it with −1,000,000, and the
//! scene walk `0x682fa0` visits the 32 depth bands nearest first: each band's entities
//! (`0x683340`) and doodad defs (`0x683700`) take the sphere test `0x686000`, its liquid slots
//! (`0x683ab0`) and terrain chunks (`0x683bf0`) the box test `0x686180`, and the band's visible
//! hole-free chunks then raise the buffer with two of their vertex rows (`0x6c15d0` →
//! `0x681b50`), so what a band hides is decided by the bands in front of it. Every step runs only
//! with world flag 0x20 (`showCull`) and the camera's forward z, `0xc7cff4`, in [−0.9, 0.9].
//!
//! The functions here are the reference's arithmetic in its own coordinates (WoW yards, +X
//! north, +Y west, +Z up) and its row-vector matrices, `m[12..15]` the translation, with the
//! 53-bit x87 intermediates rounded where the client stores a float. [`HorizonFrame`] is the
//! frame's state: the view, and the buffer as it stood before each band, so a test made after
//! the walk reads what the reference's in-order walk would have read.

use bevy::platform::collections::HashSet;
use bevy::prelude::*;

/// The buffer's columns, 0x140 (`0x685fe0`).
pub const COLUMNS: usize = 320;
/// The depth bands the scene files into (`0x67f100`), 33.33 yd each, nearest first.
pub const BANDS: usize = 32;
/// A view matrix in the reference's layout: `p' = p·M`, `m[4·row + col]`, translation in
/// `m[12..15]`.
pub type Mat = [f32; 16];
/// One frame's buffer.
pub type Columns = [f32; COLUMNS];

/// `0x685fe0` stores 0xc9742400 in every column.
pub const CLEARED: f32 = -1_000_000.0;
/// `[0x810170]`: screen x times 64 picks the column.
const COLUMN_SCALE: f32 = 64.0;
/// `[0x86861c]`: subtracted before the `fistp`.
const HALF: f32 = 0.5;
/// `+ 0xa0` after the rounding (`0x681b50`, `0x6862d4`).
const COLUMN_BIAS: i32 = 0xa0;
/// The last column, 0x13f.
const LAST_COLUMN: i32 = 0x13f;
/// `[0x810180]`: 2.7777777 (`0x4031c71c`), the least z an occluder edge's ends may have.
const OCCLUDER_NEAR_Z: f32 = f32::from_bits(0x4031_c71c);
/// `[0x80fed4]`: 50.0, the least z of a tested point; nearer is visible.
const TEST_NEAR_Z: f32 = 50.0;
/// `[0x8029d4]`: 2^-22, the least radius `0x686000` tests.
const MIN_RADIUS: f32 = f32::from_bits(0x3480_0000);
/// The camera's forward z bounds every horizon step (`0x681b79`, `0x681b90`, `0x686032`,
/// `0x68604f`, `0x6861a6`, `0x6861bd`), both ends included.
const FORWARD_Z_LIMIT: f32 = 0.9;
/// `0x680e50`: the horizontal heading is used only when its squared length is above this.
const HEADING_MIN_SQ: f32 = 0.0001;
/// `[0x810174]`: depth times 0.03 is the band.
const BAND_SCALE: f32 = 0.03;
/// The band depth, 33.333332: the far end of the occluders' depth window is the far clip less it
/// (`0x681070`), and outdoors the near end, the feed `0xc7b66c`, is 0 (`0x6811ca`).
pub const BAND_DEPTH: f32 = 33.333_332;

/// Flags bit 3 of the tests: a point nearer than z = 50 no longer makes the object visible.
pub const NO_NEAR_GATE: u32 = 8;

/// `fistp` under the round-to-nearest control word: ties to even, and a NaN or a value outside
/// `i32` stores the integer indefinite 0x80000000.
fn fistp(v: f64) -> i32 {
    let r = v.round_ties_even();
    if r.is_finite() && r >= f64::from(i32::MIN) && r <= f64::from(i32::MAX) {
        r as i32
    } else {
        i32::MIN
    }
}

/// A gate `fcomp gate; test ah,5; jnp` fails only below the gate: equal, above and NaN pass.
fn below(value: f32, gate: f32) -> bool {
    f64::from(value) < f64::from(gate)
}

/// A column from the stored `f32(64·x)`: `fistp(p − 0.5) + 0xa0`, wrapping as the `lea` does.
fn column(x64: f32) -> i32 {
    fistp(f64::from(x64) - f64::from(HALF)).wrapping_add(COLUMN_BIAS)
}

/// `TransformPoint` (`0x7bca80`): `p·M` with the w row dropped, each sum at 53 bits.
pub fn transform_point(v: [f32; 3], m: &Mat) -> [f32; 3] {
    let f = f64::from;
    [
        (f(m[8]) * f(v[2]) + f(m[4]) * f(v[1]) + f(v[0]) * f(m[0]) + f(m[12])) as f32,
        (f(m[9]) * f(v[2]) + f(m[1]) * f(v[0]) + f(m[5]) * f(v[1]) + f(m[13])) as f32,
        (f(m[10]) * f(v[2]) + f(m[2]) * f(v[0]) + f(m[6]) * f(v[1]) + f(m[14])) as f32,
    ]
}

/// The device projection the way `0x5c3cc0` builds it, from the vertical field of view: `m0` and
/// `m5` the x and y scales, `m10 = (n + f)/(f − n)`, `m14 = −2nf/(f − n)`, `m11 = 1`.
pub fn projection(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat {
    let t = (f64::from(fov_y) * 0.5).tan();
    let (n, f) = (f64::from(near), f64::from(far));
    let mut m = [0.0f32; 16];
    m[0] = (1.0 / (f64::from(aspect) * t)) as f32;
    m[5] = (1.0 / t) as f32;
    m[10] = ((n + f) / (f - n)) as f32;
    m[14] = ((n * f * -2.0) / (f - n)) as f32;
    m[11] = 1.0;
    m
}

/// `a·b` for the reference's row-vector matrices (`0x7bc6a0`), each entry at 53 bits.
fn mul(a: &Mat, b: &Mat) -> Mat {
    let mut m = [0.0f32; 16];
    for row in 0..4 {
        for col in 0..4 {
            m[4 * row + col] = (0..4)
                .map(|k| f64::from(a[4 * row + k]) * f64::from(b[4 * k + col]))
                .sum::<f64>() as f32;
        }
    }
    m
}

/// `0x680bc0`'s horizon camera: the look-at from the origin along the heading with up (0, 0, 1)
/// (`0x5c3e70`), translated by the negated eye (`0x7bdc40`), times the device projection
/// (`0x7bc6a0`). A point through it is seen by a level camera at the eye with the real lens; its
/// z is the depth along the heading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HorizonView {
    /// `0xc7b700`, what the buffer and both tests project through.
    pub view: Mat,
    /// `0xc7d280`, the device projection the sphere test takes its radius through.
    pub proj: Mat,
    /// The horizontal facing `0xc7cfb8`, unit, and its distance −n·eye: the band plane.
    band_plane: [f32; 3],
    /// `fwd.x < 0` and `fwd.y < 0`: the far side of a chunk in each axis (`0x6c15d0`,
    /// `0xc7f294`).
    far_side: [bool; 2],
}

impl HorizonView {
    /// The horizon camera for an eye and a forward vector, or `None` when the horizon does not
    /// run this frame: the forward z outside [−0.9, 0.9] (`0x681b79`; a NaN runs), or a heading
    /// too short to normalise (`0x680e50`, unreachable inside that gate, which leaves at least
    /// 0.19 of it).
    pub fn new(eye: [f32; 3], forward: [f32; 3], proj: Mat) -> Option<Self> {
        let len = forward
            .iter()
            .map(|c| f64::from(*c).powi(2))
            .sum::<f64>()
            .sqrt();
        let f = forward.map(|c| (f64::from(c) / len) as f32);
        if below(f[2], -FORWARD_Z_LIMIT) || below(FORWARD_Z_LIMIT, f[2]) {
            return None;
        }
        let h_sq = f[0] * f[0] + f[1] * f[1];
        if h_sq.partial_cmp(&HEADING_MIN_SQ) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        let h_len = f64::from(h_sq).sqrt();
        let h = [
            (f64::from(f[0]) / h_len) as f32,
            (f64::from(f[1]) / h_len) as f32,
            0.0,
        ];
        // `0x5c3e70` with the eye at the origin: s = h × up, u = s × h, rows s, u, h.
        let s_raw = [h[1], -h[0], 0.0f32];
        let s_len = f64::from(s_raw[0] * s_raw[0] + s_raw[1] * s_raw[1]).sqrt();
        let s = s_raw.map(|c| (f64::from(c) / s_len) as f32);
        let u = [
            0.0,
            0.0,
            (f64::from(s[0]) * f64::from(h[1]) - f64::from(s[1]) * f64::from(h[0])) as f32,
        ];
        let mut look = [0.0f32; 16];
        (look[0], look[4], look[8]) = (s[0], s[1], s[2]);
        (look[1], look[5], look[9]) = (u[0], u[1], u[2]);
        (look[2], look[6], look[10]) = (h[0], h[1], h[2]);
        look[15] = 1.0;
        // `0x7bdc40`: the negated eye through the 3×3 into the translation.
        let t = eye.map(|c| -c);
        for col in 0..3 {
            look[12 + col] = (f64::from(look[8 + col]) * f64::from(t[2])
                + f64::from(look[4 + col]) * f64::from(t[1])
                + f64::from(t[0]) * f64::from(look[col])
                + f64::from(look[12 + col])) as f32;
        }
        Some(Self {
            view: mul(&look, &proj),
            proj,
            band_plane: [
                h[0],
                h[1],
                -(f64::from(h[0]) * f64::from(eye[0]) + f64::from(h[1]) * f64::from(eye[1])) as f32,
            ],
            far_side: [f[0] < 0.0, f[1] < 0.0],
        })
    }

    /// The band of a point less `radius` along the horizontal facing (`0x6816f0`, `0x6818b0`):
    /// round-to-nearest of `depth·0.03 − 0.5`, a negative band 0; `None` from 32 on, which the
    /// scene does not file at all (`0x681901`).
    pub fn band(&self, p: [f32; 3], radius: f32) -> Option<usize> {
        let [nx, ny, d] = self.band_plane;
        let depth =
            (f64::from(nx) * f64::from(p[0]) + f64::from(ny) * f64::from(p[1]) + f64::from(d)
                - f64::from(radius)) as f32;
        let band = fistp(f64::from((f64::from(depth) * f64::from(BAND_SCALE)) as f32) - 0.5);
        match usize::try_from(band) {
            Err(_) => Some(0),
            Ok(b) if b < BANDS => Some(b),
            Ok(_) => None,
        }
    }
}

/// `0x681b50`'s raise over an occluder strip: each point through the view to `x/z` and `y/z`
/// with the reciprocal of z kept (`0x681c0d`); each edge whose two ends both have z of at least
/// 2.7777777 (`0x681c44`) raises every column from one end's to the other's, clamped to 0..319,
/// to the lower end's `y/z` (`0x681c69`, a tie or a NaN taking the second), a column raised only
/// when below it (`0x681ce6`, NaN not).
pub fn insert(points: &[[f32; 3]], view: &Mat, columns: &mut Columns) {
    let mut prev: Option<(f32, f32, f32)> = None;
    for p in points {
        let c = transform_point(*p, view);
        let inv_z = 1.0 / f64::from(c[2]);
        let here = (
            (inv_z * f64::from(c[0])) as f32,
            (inv_z * f64::from(c[1])) as f32,
            c[2],
        );
        if let Some((x0, y0, z0)) = prev.replace(here) {
            let (x1, y1, z1) = here;
            if below(z0, OCCLUDER_NEAR_Z) || below(z1, OCCLUDER_NEAR_Z) {
                continue;
            }
            let y = if f64::from(y0) < f64::from(y1) {
                y0
            } else {
                y1
            };
            let c0 = column((f64::from(COLUMN_SCALE) * f64::from(x0)) as f32);
            let c1 = column((f64::from(COLUMN_SCALE) * f64::from(x1)) as f32);
            let (lo, hi) = (c0.min(c1).max(0), c0.max(c1).min(LAST_COLUMN));
            for col in lo..=hi {
                let slot = &mut columns[col as usize];
                if f64::from(*slot) < f64::from(y) {
                    *slot = y;
                }
            }
        }
    }
}

/// The span walk shared by both tests: hidden when every column from `lo` to `hi`, clamped, is
/// at least `top`; a span starting at 320 or ending below 0 is visible, and so is the first
/// column below `top` (`0x686150`, `0x686316`; a NaN is not below).
fn span_hidden(lo: i32, hi: i32, top: f32, columns: &Columns) -> bool {
    if lo >= COLUMNS as i32 || hi < 0 {
        return false;
    }
    (lo.max(0)..=hi.min(LAST_COLUMN)).all(|c| !below(columns[c as usize], top))
}

/// `0x686000`: whether a sphere is hidden behind the horizon (the test's 2). A radius below 2^-22
/// is visible, and without [`NO_NEAR_GATE`] so is a centre nearer than z = 50 (`0x6860a4`). With
/// `(r, r, 0)` through the projection (`0x686086`), the top is `y/z` plus the radius's first term
/// over z, and the columns run from `col(64·(x/z − ry/z))` to `col(64·(x/z + ry/z)) + 1`
/// (`0x6860e9` to `0x686124`): the vertical reach takes the x scale and the span the y scale.
pub fn sphere_hidden(
    center: [f32; 3],
    radius: f32,
    flags: u32,
    view: &HorizonView,
    columns: &Columns,
) -> bool {
    if below(radius.abs(), MIN_RADIUS) {
        return false;
    }
    let c = transform_point(center, &view.view);
    let r = transform_point([radius, radius, 0.0], &view.proj);
    if flags & NO_NEAR_GATE == 0 && below(c[2], TEST_NEAR_Z) {
        return false;
    }
    let inv_z = 1.0 / f64::from(c[2]);
    let cx = (inv_z * f64::from(c[0])) as f32;
    let rx = (inv_z * f64::from(r[0])) as f32;
    let ry = (inv_z * f64::from(r[1])) as f32;
    let top = (f64::from(c[1]) * inv_z + f64::from(rx)) as f32;
    let lo_x = (f64::from(cx) - f64::from(ry)) as f32;
    let hi_x = (f64::from(ry) + f64::from(cx)) as f32;
    let lo = column((f64::from(lo_x) * f64::from(COLUMN_SCALE)) as f32);
    let hi = column((f64::from(hi_x) * f64::from(COLUMN_SCALE)) as f32).wrapping_add(1);
    span_hidden(lo, hi, top, columns)
}

/// `0x686180`: whether a box is hidden behind the horizon (the test's 2). Its eight corners go
/// through the view; without [`NO_NEAR_GATE`] a corner nearer than z = 50 makes it visible
/// (`0x68623f`). Over the corners' `x/z` and `y/z` it keeps the least x and the greatest x and y,
/// and the span runs from `col(64·min x)` to `col(64·max x) + 1` (`0x6862d4`, `0x6862f1`), one
/// more than the occluders' rule.
pub fn box_hidden(
    min: [f32; 3],
    max: [f32; 3],
    flags: u32,
    view: &HorizonView,
    columns: &Columns,
) -> bool {
    // The corner tables `0x868628` (x), `0x868648` (y) and `0x868668` (z): 0 is min, 1 max.
    const X: [usize; 8] = [0, 1, 1, 0, 0, 1, 1, 0];
    const Y: [usize; 8] = [0, 0, 1, 1, 0, 0, 1, 1];
    const Z: [usize; 8] = [0, 0, 0, 0, 1, 1, 1, 1];
    let ends = [min, max];
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for i in 0..8 {
        let corner = [ends[X[i]][0], ends[Y[i]][1], ends[Z[i]][2]];
        let p = transform_point(corner, &view.view);
        if flags & NO_NEAR_GATE == 0 && below(p[2], TEST_NEAR_Z) {
            return false;
        }
        let inv_z = 1.0 / f64::from(p[2]);
        let sx = (f64::from(p[0]) * inv_z) as f32;
        let sy = (f64::from(p[1]) * inv_z) as f32;
        if f64::from(sx) < f64::from(min_x) {
            min_x = sx;
        }
        if f64::from(max_x) < f64::from(sx) {
            max_x = sx;
        }
        if f64::from(max_y) < f64::from(sy) {
            max_y = sy;
        }
    }
    let lo = column((f64::from(COLUMN_SCALE) * f64::from(min_x)) as f32);
    let hi = column((f64::from(COLUMN_SCALE) * f64::from(max_x)) as f32).wrapping_add(1);
    span_hidden(lo, hi, max_y, columns)
}

/// `0x6c15d0`'s two rows of a chunk's outer grid by its level: the vertex count `[0x810f60 +
/// 4·level]` (2, 3, 5, 9) and the step `[0x810f70 + 4·level]` (8, 4, 2, 1).
pub fn level_rows(level: usize) -> (usize, usize) {
    const COUNT: [usize; 4] = [2, 3, 5, 9];
    const STEP: [usize; 4] = [8, 4, 2, 1];
    let level = level.min(3);
    (COUNT[level], STEP[level])
}

/// The chunk level the occluder rows take. A chunk is level 3 (`maxLOD`, `[0x867964]`) unless
/// the `lod` option sends one past `lodDist` to 2 (`0x6afb1d`); benilla draws every chunk at the
/// full 145-vertex mesh, so its rows are level 3's.
pub const CHUNK_LEVEL: usize = 3;

/// One terrain chunk as the horizon reads it, in WoW yards: the box `0x6b0e50` stores (`+0x44`,
/// `+0x50`), its sphere (`+0x5c`, `+0x68`), the hole word (MCNK `+0x3c`), and the four edges of
/// the outer grid, the only vertices `0x6c15d0` ever adds.
#[derive(Clone, Debug)]
pub struct HorizonChunk {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub center: [f32; 3],
    pub radius: f32,
    pub holes: u16,
    /// Outer row 0 (vertices 0..=8) and row 8 (0x88..=0x90), west to east along +Y's negation.
    rows: [[[f32; 3]; 9]; 2],
    /// Outer column 0 (vertices 0, 17, …, 0x88) and column 8 (8, 25, …, 0x90).
    cols: [[[f32; 3]; 9]; 2],
}

impl HorizonChunk {
    /// From a chunk's 145 vertices in stride-17 order (outer vertex `(r, c)` at `17r + c`), or
    /// `None` for a short list. The box and sphere follow `0x6b0e50`: min and max of all 145, the
    /// centre their mean, the radius `|max − centre|`.
    pub fn new(positions: &[[f32; 3]], holes: u16) -> Option<Self> {
        if positions.len() < 145 {
            return None;
        }
        let mut min = positions[0];
        let mut max = positions[0];
        for p in &positions[..145] {
            for a in 0..3 {
                min[a] = min[a].min(p[a]);
                max[a] = max[a].max(p[a]);
            }
        }
        let center = [0, 1, 2].map(|a| (min[a] + max[a]) * 0.5);
        let radius = (0..3)
            .map(|a| f64::from(max[a] - center[a]).powi(2))
            .sum::<f64>()
            .sqrt() as f32;
        let outer = |r: usize, c: usize| positions[17 * r + c];
        Some(Self {
            min,
            max,
            center,
            radius,
            holes,
            rows: [0, 8].map(|r| std::array::from_fn(|c| outer(r, c))),
            cols: [0, 8].map(|c| std::array::from_fn(|r| outer(r, c))),
        })
    }

    /// The corner `0x6afca6` files the chunk by: vertex 0, 8, 0x88 or 0x90 (`0x86b580`), the
    /// far one in each axis from the camera's heading.
    pub fn band_corner(&self, view: &HorizonView) -> [f32; 3] {
        let [x_far, y_far] = view.far_side;
        self.rows[usize::from(x_far)][if y_far { 8 } else { 0 }]
    }

    /// `0x6c15d0`'s two strips at `level`: outer row 0x88 when the camera target's x is below the
    /// eye's, else row 0 (`0x6c1604`), then column 8 when its y is below, else column 0
    /// (`0x6c164d`), each every `step` vertices.
    pub fn occluder_rows(&self, level: usize, view: &HorizonView) -> [Vec<[f32; 3]>; 2] {
        let (count, step) = level_rows(level);
        let [x_far, y_far] = view.far_side;
        let pick = |edge: &[[f32; 3]; 9]| (0..count).map(|k| edge[k * step]).collect();
        [
            pick(&self.rows[usize::from(x_far)]),
            pick(&self.cols[usize::from(y_far)]),
        ]
    }
}

/// A terrain cell's chunks, beside its mesh: the horizon walks chunks, benilla draws cells of
/// them, and a cell is hidden only when none of its chunks draws.
#[derive(Component, Clone, Debug)]
pub struct HorizonCell(pub Vec<HorizonChunk>);

/// This frame's horizon: the view and the buffer before each band, or inactive. Inactive admits
/// everything, as a clear world flag 0x20 makes every test report visible.
#[derive(Resource, Default, Clone, PartialEq)]
pub struct HorizonFrame {
    pub(crate) view: Option<HorizonView>,
    /// `before[b]` is the buffer the walk had when it reached band `b`; the last entry is the
    /// buffer after band 31.
    pub(crate) before: Vec<Columns>,
    /// Terrain cells every drawn-or-hidden chunk of which the horizon hid.
    pub(crate) hidden_cells: HashSet<Entity>,
}

impl HorizonFrame {
    /// The buffer an object in `band` was tested against; past the bands, the final one.
    fn columns(&self, band: Option<usize>) -> Option<&Columns> {
        let b = band.unwrap_or(BANDS).min(self.before.len().checked_sub(1)?);
        self.before.get(b)
    }

    /// `0x683340`/`0x683700`'s horizon term for a sphere in WoW yards, filed by its depth less
    /// the radius; flags 0.
    pub fn sphere_hidden(&self, center: [f32; 3], radius: f32) -> bool {
        let Some(view) = &self.view else {
            return false;
        };
        self.columns(view.band(center, radius))
            .is_some_and(|cols| sphere_hidden(center, radius, 0, view, cols))
    }

    /// `0x683ab0`'s horizon term for a box in WoW yards, filed by its centre (`0x681970`); flags
    /// 0.
    pub fn box_hidden(&self, min: [f32; 3], max: [f32; 3]) -> bool {
        let Some(view) = &self.view else {
            return false;
        };
        let center = [0, 1, 2].map(|a| (min[a] + max[a]) * 0.5);
        self.columns(view.band(center, 0.0))
            .is_some_and(|cols| box_hidden(min, max, 0, view, cols))
    }

    /// Whether the horizon hid every chunk of a terrain cell that the frustum let through.
    pub fn cell_hidden(&self, cell: Entity) -> bool {
        self.hidden_cells.contains(&cell)
    }

    /// Whether the horizon runs this frame.
    pub fn active(&self) -> bool {
        self.view.is_some()
    }
}

/// What the horizon did on its last frame, for the A/B line.
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HorizonCounts {
    /// Chunks the walk reached in a band and the frustum let through, and how many it hid.
    pub chunks: usize,
    pub chunks_hidden: usize,
    /// Chunks whose rows raised the buffer.
    pub occluders: usize,
    /// Terrain cells, outdoor net bodies, open-world liquid surfaces and doodad submeshes hidden.
    pub cells_hidden: usize,
    pub bodies_hidden: usize,
    pub liquid_hidden: usize,
    pub doodad_parts_hidden: usize,
    /// The chunk pass's wall time, µs.
    pub build_us: u64,
}

/// A flat chunk at height `z` whose outer vertex (0, 0) is at (x0, y0), rows stepping down X and
/// columns down Y on the 4.1666 yd lattice, as `adt_to_tile_mesh` lays them out.
#[cfg(test)]
pub(crate) fn flat_chunk(x0: f32, y0: f32, z: f32) -> HorizonChunk {
    let unit = BAND_DEPTH / 8.0;
    let mut p = Vec::with_capacity(145);
    for r in 0..9 {
        for c in 0..9 {
            p.push([x0 - r as f32 * unit, y0 - c as f32 * unit, z]);
        }
        if r < 8 {
            for c in 0..8 {
                p.push([
                    x0 - (r as f32 + 0.5) * unit,
                    y0 - (c as f32 + 0.5) * unit,
                    z,
                ]);
            }
        }
    }
    HorizonChunk::new(&p, 0).expect("145 vertices")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// benilla-re's `sample_view` for the difftests: near the identity with a tilt.
    const SAMPLE_VIEW: Mat = [
        1.0, 0.0, 0.1, 0.0, //
        0.2, 1.0, 0.0, 0.0, //
        0.0, 0.15, 1.0, 0.0, //
        0.3, -0.2, 2.0, 0.0,
    ];
    /// benilla-re's `sample_radius_mat`: half the radius, no translation.
    const SAMPLE_RADIUS: Mat = [
        0.5, 0.0, 0.0, 0.0, //
        0.0, 0.5, 0.0, 0.0, //
        0.0, 0.0, 0.0, 0.0, //
        0.0, 0.0, 0.0, 0.0,
    ];
    const IDENTITY: Mat = [
        1.0, 0.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, //
        0.0, 0.0, 0.0, 1.0,
    ];

    fn view_of(view: Mat, proj: Mat) -> HorizonView {
        HorizonView {
            view,
            proj,
            band_plane: [1.0, 0.0, 0.0],
            far_side: [false, false],
        }
    }

    /// The difftests' ramp skyline: column j at `0.01·j − 1.5`.
    fn ramp() -> Columns {
        std::array::from_fn(|j| j as f32 * 0.01 - 1.5)
    }

    #[test]
    fn fistp_rounds_ties_to_even_and_stores_the_indefinite_out_of_range() {
        assert_eq!(fistp(0.5), 0);
        assert_eq!(fistp(1.5), 2);
        assert_eq!(fistp(-0.5), 0);
        assert_eq!(fistp(-1.5), -2);
        assert_eq!(fistp(f64::NAN), i32::MIN);
        assert_eq!(fistp(3.0e9), i32::MIN);
        assert_eq!(fistp(-1.0e30), i32::MIN);
    }

    /// The column rule: screen x 0 is column 160, one column is 1/64, and `−0.5` then ties to
    /// even put x = 1/128 in column 160.
    #[test]
    fn a_column_is_sixty_four_per_unit_of_screen_x_about_160() {
        assert_eq!(column(0.0), 160);
        assert_eq!(column(64.0), 224);
        assert_eq!(column(-64.0), 96);
        assert_eq!(column(0.5), 160, "0.5 − 0.5 = 0");
        assert_eq!(column(1.5), 161, "1.5 − 0.5 = 1");
        assert_eq!(column(1.0), 160, "0.5 ties to even");
        // A NaN x wraps the indefinite: far below 0, so the clamps take it to column 0.
        assert!(column(f32::NAN) < 0);
    }

    /// `0x681b50` on a strip through the identity view: the edge from x/z = −0.5 to 0.5 at
    /// y/z = 0.2 and 0.1 raises columns 128..=192 to the lower, 0.1.
    #[test]
    fn an_occluder_edge_raises_its_columns_to_the_lower_end() {
        let mut cols = [CLEARED; COLUMNS];
        insert(&[[-5.0, 2.0, 10.0], [5.0, 1.0, 10.0]], &IDENTITY, &mut cols);
        // col(64·−0.5) = fistp(−32.5) + 160 = −32 + 160 = 128; col(32) = fistp(31.5) + 160 = 192.
        for (c, v) in cols.iter().enumerate() {
            let want = if (128..=192).contains(&c) {
                0.1
            } else {
                CLEARED
            };
            assert_eq!(*v, want, "column {c}");
        }
        // A lower occluder later does not lower a column.
        insert(
            &[[-5.0, -2.0, 10.0], [5.0, -2.0, 10.0]],
            &IDENTITY,
            &mut cols,
        );
        assert_eq!(cols[160], 0.1);
    }

    /// An edge with either end nearer than 2.7777777 adds nothing; exactly at the gate it does.
    #[test]
    fn an_occluder_edge_needs_both_ends_past_the_near_gate() {
        let mut cols = [CLEARED; COLUMNS];
        insert(&[[0.0, 1.0, 2.7], [1.0, 1.0, 10.0]], &IDENTITY, &mut cols);
        assert!(cols.iter().all(|v| *v == CLEARED));
        insert(
            &[[0.0, 1.0, OCCLUDER_NEAR_Z], [1.0, 1.0, 10.0]],
            &IDENTITY,
            &mut cols,
        );
        assert!(cols.iter().any(|v| *v != CLEARED));
    }

    /// The span clamps to 0..319: an edge off both sides of the screen fills every column.
    #[test]
    fn an_occluder_span_clamps_to_the_buffer() {
        let mut cols = [CLEARED; COLUMNS];
        insert(
            &[[-100.0, 0.0, 10.0], [100.0, 0.0, 10.0]],
            &IDENTITY,
            &mut cols,
        );
        assert!(cols.iter().all(|v| *v == 0.0));
    }

    /// A NaN y keeps the second end's (`0x681c69`), and a NaN column never compares below.
    #[test]
    fn non_finite_occluders_take_the_second_end_and_leave_nan_columns() {
        let mut cols = [CLEARED; COLUMNS];
        insert(
            &[[0.0, f32::NAN, 10.0], [1.0, 1.0, 10.0]],
            &IDENTITY,
            &mut cols,
        );
        assert_eq!(cols[160], 0.1, "the NaN y loses to the second end's 0.1");
        let mut nan = [f32::NAN; COLUMNS];
        insert(&[[0.0, 1.0, 10.0], [1.0, 1.0, 10.0]], &IDENTITY, &mut nan);
        assert!(nan[160].is_nan(), "a NaN column is never below the value");
    }

    /// `0x686180`: a box behind a high skyline is hidden; one reaching above it, a cleared buffer
    /// or a corner nearer than 50 is visible, the last unless flags bit 3.
    #[test]
    fn the_box_test_reads_the_highest_corner_over_its_span() {
        let view = view_of(IDENTITY, IDENTITY);
        let wall = [0.5f32; COLUMNS];
        let low = ([-1.0, -1.0, 60.0], [1.0, 1.0, 90.0]); // y/z at most 1/60
        assert!(box_hidden(low.0, low.1, 0, &view, &wall));
        assert!(!box_hidden(low.0, low.1, 0, &view, &[CLEARED; COLUMNS]));
        let tall = ([-1.0, -1.0, 60.0], [1.0, 40.0, 90.0]); // top 40/60
        assert!(!box_hidden(tall.0, tall.1, 0, &view, &wall));
        let near = ([-1.0, -1.0, 40.0], [1.0, 1.0, 90.0]);
        assert!(!box_hidden(near.0, near.1, 0, &view, &wall));
        assert!(box_hidden(near.0, near.1, NO_NEAR_GATE, &view, &wall));
    }

    /// The box span is one wider than the occluders' (`+ 0xa1`): a single low column at the
    /// right of a box's own span still shows it.
    #[test]
    fn the_box_span_reaches_one_column_past_its_max_x() {
        let view = view_of(IDENTITY, IDENTITY);
        // x/z = ±0.25/60, 64 times it ±0.267: columns 159 and 160, the span 159..=161.
        let b = ([-0.25, -1.0, 60.0], [0.25, 1.0, 60.0]);
        let mut cols = [0.5f32; COLUMNS];
        assert!(box_hidden(b.0, b.1, 0, &view, &cols));
        cols[161] = CLEARED;
        assert!(
            !box_hidden(b.0, b.1, 0, &view, &cols),
            "column 161 is in the span"
        );
        cols[161] = 0.5;
        cols[162] = CLEARED;
        assert!(box_hidden(b.0, b.1, 0, &view, &cols), "column 162 is not");
    }

    /// A span wholly off the buffer is visible, whatever the columns hold.
    #[test]
    fn a_span_off_the_buffer_is_visible() {
        let view = view_of(IDENTITY, IDENTITY);
        let all = [f32::MAX; COLUMNS];
        // x/z = 3: column 352, past 319.
        assert!(!box_hidden(
            [180.0, 0.0, 60.0],
            [181.0, 0.0, 60.0],
            0,
            &view,
            &all
        ));
        assert!(!box_hidden(
            [-181.0, 0.0, 60.0],
            [-180.0, 0.0, 60.0],
            0,
            &view,
            &all
        ));
    }

    /// NaN probes of `horizon_non_finite_probes`: a NaN column is never below the top, so an
    /// all-NaN skyline hides; a cleared one never does.
    #[test]
    fn non_finite_columns_hide_and_the_cleared_buffer_never_does() {
        let view = view_of(SAMPLE_VIEW, SAMPLE_RADIUS);
        let base = ([-1.0, -1.0, 60.0], [1.0, 1.0, 90.0]);
        assert!(box_hidden(base.0, base.1, 0, &view, &[f32::NAN; COLUMNS]));
        assert!(!box_hidden(base.0, base.1, 0, &view, &[CLEARED; COLUMNS]));
        assert!(sphere_hidden(
            [0.5, 0.25, 75.0],
            5.0,
            0,
            &view,
            &[f32::NAN; COLUMNS]
        ));
        assert!(!sphere_hidden(
            [0.5, 0.25, 75.0],
            5.0,
            0,
            &view,
            &[CLEARED; COLUMNS]
        ));
    }

    /// `0x686000` on the probes' base sphere over the raised ramp (`horizon_span_test`'s
    /// `case % 5 == 1`): the top is `y/z + 0.5r/z`, the span `x/z ± 0.5r/z`. Worked: centre
    /// (0.5, 0.25, 75) through the sample view is (0.85, 11.3, 77.05), r (2.5, 2.5); top ≈
    /// 0.1467 + 0.0324 = 0.179, the span x/z 0.0110 ± 0.0324 → columns 158..=163, where the ramp
    /// is 0.38..0.43: hidden.
    #[test]
    fn the_sphere_test_hides_below_the_ramp_and_shows_above_it() {
        let view = view_of(SAMPLE_VIEW, SAMPLE_RADIUS);
        let cols = ramp().map(|v| v + 0.3);
        assert!(sphere_hidden([0.5, 0.25, 75.0], 5.0, 0, &view, &cols));
        // Raised by 30 yd, the top (≈ 0.57) clears the span's 0.43.
        assert!(!sphere_hidden([0.5, 30.25, 75.0], 5.0, 0, &view, &cols));
        // One low column inside the span shows it.
        let mut dent = cols;
        dent[160] = CLEARED;
        assert!(!sphere_hidden([0.5, 0.25, 75.0], 5.0, 0, &view, &dent));
    }

    /// The sphere's gates: a radius under 2^-22 and a centre nearer than z = 50 are visible, the
    /// second unless flags bit 3; a negative radius is tested by its magnitude.
    #[test]
    fn the_sphere_test_gates_on_radius_and_near_z() {
        let view = view_of(IDENTITY, SAMPLE_RADIUS);
        let wall = [1.0f32; COLUMNS];
        assert!(sphere_hidden([0.0, 0.0, 60.0], 1.0, 0, &view, &wall));
        assert!(!sphere_hidden([0.0, 0.0, 60.0], 1e-9, 0, &view, &wall));
        assert!(sphere_hidden([0.0, 0.0, 60.0], -1.0, 0, &view, &wall));
        assert!(!sphere_hidden([0.0, 0.0, 40.0], 1.0, 0, &view, &wall));
        assert!(sphere_hidden(
            [0.0, 0.0, 40.0],
            1.0,
            NO_NEAR_GATE,
            &view,
            &wall
        ));
    }

    /// The level camera looks along the heading from the eye: a point straight ahead projects to
    /// x = 0, y = 0 at the eye's height, z its distance through the projection's depth terms.
    #[test]
    fn the_horizon_view_is_level_along_the_heading() {
        let proj = projection(std::f32::consts::FRAC_PI_4, 16.0 / 9.0, 0.1, 350.0);
        let eye = [100.0, 200.0, 50.0];
        // Facing north-west and pitched down: the horizon camera ignores the pitch.
        let v = HorizonView::new(eye, [1.0, 1.0, -0.8], proj).unwrap();
        let d = 100.0f32;
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let ahead = transform_point([eye[0] + d * h, eye[1] + d * h, eye[2]], &v.view);
        assert!(ahead[0].abs() < 1e-3 && ahead[1].abs() < 1e-3, "{ahead:?}");
        let want_z = proj[10] * d + proj[14];
        assert!((ahead[2] - want_z).abs() < 1e-2, "{ahead:?} vs {want_z}");
        // +Z is up on screen, and the right hand (east of a north-west heading) is +x.
        let up = transform_point([eye[0] + d * h, eye[1] + d * h, eye[2] + 10.0], &v.view);
        assert!(up[1] > 0.0);
        let right = transform_point(
            [eye[0] + d * h + 10.0, eye[1] + d * h - 10.0, eye[2]],
            &v.view,
        );
        assert!(right[0] > 0.0, "{right:?}");
    }

    /// Outside [−0.9, 0.9] of forward z the horizon does not run.
    #[test]
    fn a_steep_camera_turns_the_horizon_off() {
        let proj = projection(std::f32::consts::FRAC_PI_4, 1.0, 0.1, 350.0);
        let at = |z: f32| {
            let h = (1.0 - z * z).sqrt();
            HorizonView::new([0.0; 3], [h, 0.0, z], proj)
        };
        assert!(at(0.0).is_some());
        assert!(at(0.89).is_some());
        assert!(at(-0.89).is_some());
        assert!(at(0.91).is_none());
        assert!(at(-0.91).is_none());
        // The gate's own compare passes the end and a NaN.
        assert!(!below(0.9, -FORWARD_Z_LIMIT) && !below(FORWARD_Z_LIMIT, 0.9));
        assert!(!below(f32::NAN, -FORWARD_Z_LIMIT) && !below(FORWARD_Z_LIMIT, f32::NAN));
    }

    /// Bands are 33.33 yd of horizontal depth less the radius, nearest 0, unfiled from 32.
    #[test]
    fn bands_follow_the_horizontal_facing() {
        let proj = projection(std::f32::consts::FRAC_PI_4, 1.0, 0.1, 350.0);
        let v = HorizonView::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], proj).unwrap();
        assert_eq!(v.band([10.0, 0.0, 0.0], 0.0), Some(0));
        assert_eq!(v.band([-50.0, 0.0, 0.0], 0.0), Some(0), "behind is band 0");
        // 0.03·50 − 0.5 = 1.0.
        assert_eq!(v.band([50.0, 0.0, 0.0], 0.0), Some(1));
        assert_eq!(
            v.band([60.0, 7.0, 99.0], 10.0),
            Some(1),
            "less the radius; z and y ignored"
        );
        assert_eq!(v.band([990.0, 0.0, 0.0], 0.0), Some(29));
        assert_eq!(v.band([1100.0, 0.0, 0.0], 0.0), None);
    }

    /// `0x6c15d0`'s tables: level 3 adds nine vertices a row, level 2 five, every other.
    #[test]
    fn the_occluder_rows_follow_the_level_tables() {
        assert_eq!(level_rows(3), (9, 1));
        assert_eq!(level_rows(2), (5, 2));
        assert_eq!(level_rows(1), (3, 4));
        assert_eq!(level_rows(0), (2, 8));
    }

    use super::flat_chunk as chunk_at;

    /// The rows are the far edges from the heading: facing +X (north) and +Y (west) they are
    /// row 0 and column 0; facing −X and −Y, row 8 (vertex 0x88 on) and column 8.
    #[test]
    fn the_occluder_rows_are_the_chunks_far_edges() {
        let proj = projection(std::f32::consts::FRAC_PI_4, 1.0, 0.1, 350.0);
        let c = chunk_at(100.0, 100.0, 0.0);
        let unit = BAND_DEPTH / 8.0;
        let north_west = HorizonView::new([0.0; 3], [1.0, 1.0, 0.0], proj).unwrap();
        let [row, col] = c.occluder_rows(3, &north_west);
        assert_eq!(row.len(), 9);
        assert!(row.iter().all(|p| p[0] == 100.0), "row 0 is the north edge");
        assert!(
            col.iter().all(|p| p[1] == 100.0),
            "column 0 is the west edge"
        );
        assert_eq!(c.band_corner(&north_west), [100.0, 100.0, 0.0]);
        let south_east = HorizonView::new([200.0, 200.0, 0.0], [-1.0, -1.0, 0.0], proj).unwrap();
        let [row, col] = c.occluder_rows(2, &south_east);
        assert_eq!(row.len(), 5);
        assert!(row
            .iter()
            .all(|p| (p[0] - (100.0 - 8.0 * unit)).abs() < 1e-3));
        assert!(col
            .iter()
            .all(|p| (p[1] - (100.0 - 8.0 * unit)).abs() < 1e-3));
        assert_eq!(row[1][1], 100.0 - 2.0 * unit, "level 2 steps two vertices");
    }

    /// The box and sphere of `0x6b0e50`: min and max of the vertices, the centre their mean, the
    /// radius the half diagonal.
    #[test]
    fn a_chunks_sphere_is_its_boxs() {
        let c = chunk_at(100.0, 50.0, 7.0);
        assert_eq!(c.max, [100.0, 50.0, 7.0]);
        assert!((c.min[0] - (100.0 - BAND_DEPTH)).abs() < 1e-3);
        let half = BAND_DEPTH * 0.5;
        assert!((c.radius - (2.0 * half * half).sqrt()).abs() < 1e-3);
    }

    /// End to end: a ridge 100 yd ahead, raised 20 yd above the eye, hides a low box 300 yd ahead
    /// and not one standing 200 yd above the ground there.
    #[test]
    fn a_near_ridge_hides_the_low_ground_behind_it() {
        let proj = projection(std::f32::consts::FRAC_PI_4, 16.0 / 9.0, 0.1, 350.0);
        let v = HorizonView::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], proj).unwrap();
        let ridge = chunk_at(130.0, 16.0, 20.0);
        let mut cols = [CLEARED; COLUMNS];
        for row in ridge.occluder_rows(3, &v) {
            insert(&row, &v.view, &mut cols);
        }
        assert!(box_hidden(
            [300.0, -5.0, -5.0],
            [310.0, 5.0, 5.0],
            0,
            &v,
            &cols
        ));
        assert!(!box_hidden(
            [300.0, -5.0, 195.0],
            [310.0, 5.0, 205.0],
            0,
            &v,
            &cols
        ));
        // Off to the side of the ridge's columns it shows.
        assert!(!box_hidden(
            [300.0, 200.0, -5.0],
            [310.0, 210.0, 5.0],
            0,
            &v,
            &cols
        ));
        assert!(sphere_hidden([305.0, 0.0, 0.0], 5.0, 0, &v, &cols));
    }
}
