//! Software renderer for one frame of a burst (Windows overlay).
//! Output: premultiplied BGRA pixels (`0xAARRGGBB` as u32), ready for
//! `UpdateLayeredWindow`.
//!
//! Soft styles (smoke, glow, rift) are shaded per pixel on a few threads, at
//! half resolution for big bursts, then scaled up; sharp details (sparks,
//! streaks) are drawn at full resolution on top.

use ddc_core::config::EffectStyle;

use crate::{Edge, Kind};

/// Deterministic pseudo-random number in 0..1 (no RNG dependency).
pub fn rnd(seed: u32, i: u32, k: u32) -> f32 {
    let mut h = seed ^ i.wrapping_mul(0x9E37_79B9) ^ k.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    h as f32 / u32::MAX as f32
}

pub const PARTICLES: u32 = 40;

/// Particle `i`: (direction x, direction y, speed 0.45..1, size factor 0.6..1.4).
pub fn particle(seed: u32, i: u32) -> (f32, f32, f32, f32) {
    let angle = rnd(seed, i, 1) * std::f32::consts::TAU;
    (angle.cos(), angle.sin(), 0.45 + 0.55 * rnd(seed, i, 2), 0.6 + 0.8 * rnd(seed, i, 3))
}

pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 0 → 1 → 0 bump centred on `at`.
fn bump(x: f32, at: f32, width: f32) -> f32 {
    (-((x - at) / width).powi(2)).exp()
}

/// Premultiplied pixel being built up in floats (colour 0..255, alpha 0..1).
#[derive(Clone, Copy, Default)]
struct Px {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl Px {
    /// Normal "over" compositing.
    fn over(&mut self, a: f32, rgb: (f32, f32, f32)) {
        let a = a.clamp(0.0, 1.0);
        let k = 1.0 - a;
        self.r = rgb.0 * a + self.r * k;
        self.g = rgb.1 * a + self.g * k;
        self.b = rgb.2 * a + self.b * k;
        self.a = a + self.a * k;
    }

    /// Light: colour adds up, coverage combines like "over".
    fn add(&mut self, a: f32, rgb: (f32, f32, f32)) {
        let a = a.max(0.0);
        if a <= 0.001 {
            return;
        }
        self.r += rgb.0 * a;
        self.g += rgb.1 * a;
        self.b += rgb.2 * a;
        self.a += a.min(1.0) * (1.0 - self.a);
    }

    fn pack(self) -> u32 {
        let a = self.a.clamp(0.0, 1.0);
        let max = a * 255.0;
        // premultiplied: colour never exceeds alpha
        let c = |v: f32| v.min(max).round().clamp(0.0, 255.0) as u32;
        (c(max) << 24) | (c(self.r) << 16) | (c(self.g) << 8) | c(self.b)
    }

    fn unpack(p: u32) -> Self {
        Self {
            a: (p >> 24) as f32 / 255.0,
            r: ((p >> 16) & 0xff) as f32,
            g: ((p >> 8) & 0xff) as f32,
            b: (p & 0xff) as f32,
        }
    }
}

/// Premultiplied "over": src onto dst.
fn over(dst: u32, a: f32, rgb: (f32, f32, f32)) -> u32 {
    if a <= 0.002 {
        return dst;
    }
    let mut p = Px::unpack(dst);
    p.over(a, rgb);
    p.pack()
}

fn mix_white(rgb: (u8, u8, u8), w: f32) -> (f32, f32, f32) {
    let m = |v: u8| v as f32 + (255.0 - v as f32) * w.clamp(0.0, 1.0);
    (m(rgb.0), m(rgb.1), m(rgb.2))
}

fn lerp3(a: (f32, f32, f32), b: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

/// A second, contrasting hue for chromatic fringes: the colour's channels
/// rotated, saturated a little. Near-white colours get a cool violet.
fn accent(rgb: (u8, u8, u8)) -> (f32, f32, f32) {
    let (r, g, b) = (rgb.0 as f32, rgb.1 as f32, rgb.2 as f32);
    let spread = r.max(g).max(b) - r.min(g).min(b);
    if spread < 40.0 {
        return (170.0, 120.0, 255.0);
    }
    let rot = (b, r, g);
    let mean = (rot.0 + rot.1 + rot.2) / 3.0;
    let s = |v: f32| (mean + (v - mean) * 1.25).clamp(0.0, 255.0);
    (s(rot.0), s(rot.1), s(rot.2))
}

/// Smooth 2D value noise in 0..1.
fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (fx, fy) = (x - xi, y - yi);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let h = |i: f32, j: f32| rnd(seed, (i as i32 as u32).wrapping_mul(73_856_093) ^ (j as i32 as u32).wrapping_mul(19_349_663), 9);
    let a = h(xi, yi) + (h(xi + 1.0, yi) - h(xi, yi)) * sx;
    let b = h(xi, yi + 1.0) + (h(xi + 1.0, yi + 1.0) - h(xi, yi + 1.0)) * sx;
    a + (b - a) * sy
}

/// Two rotated octaves of value noise (the rotation hides the grid).
fn fbm2(x: f32, y: f32, seed: u32) -> f32 {
    let (ax, ay) = (0.866 * x - 0.5 * y, 0.5 * x + 0.866 * y);
    let (bx, by) = (0.342 * x + 0.94 * y, -0.94 * x + 0.342 * y);
    0.65 * value_noise(ax, ay, seed) + 0.35 * value_noise(bx * 2.1, by * 2.1, seed ^ 0xABCD)
}

/// Number of mist puffs in the smoke style.
pub const PUFFS: u32 = 14;

/// Puff `i` of the smoke style, relative to the burst radius R:
/// (start x, start y, end x, end y, start radius, end radius, weight).
/// `inward` points into the screen; puffs drift that way and a bit upwards.
pub fn puff(seed: u32, i: u32, inward: (f32, f32)) -> (f32, f32, f32, f32, f32, f32, f32) {
    let a = rnd(seed, i, 11) * std::f32::consts::TAU;
    let d = 0.14 * rnd(seed, i, 12).sqrt();
    let (sx, sy) = (a.cos() * d, a.sin() * d);
    let spread = 0.2 + 0.35 * rnd(seed, i, 13);
    let inner = 0.08 + 0.25 * rnd(seed, i, 14);
    let ex = sx * (1.0 + 1.6 * spread) + inward.0 * inner;
    let ey = sy * (1.0 + 1.6 * spread) + inward.1 * inner - 0.1 * rnd(seed, i, 15);
    let r0 = 0.06 + 0.08 * rnd(seed, i, 16);
    let r1 = r0 * (1.5 + 0.8 * rnd(seed, i, 17));
    let w = 0.7 + 0.6 * rnd(seed, i, 18);
    (sx, sy, ex, ey, r0, r1, w)
}

/// Number of light streaks in the rift style.
pub const STREAKS: u32 = 34;

/// Soft styles are shaded at half resolution from this size up and scaled up
/// (they have no sharp detail, and it makes them ~4x cheaper).
const HALF_RES_FROM: usize = 200;

/// Renders frame `t` (0..1) of a burst into the top-left `size`×`size`
/// pixels of `buf` (row stride `stride` pixels). The burst centre is the
/// middle of that square; `edge` is the screen edge it sits on.
#[allow(clippy::too_many_arguments)]
pub fn render(
    buf: &mut [u32],
    stride: usize,
    size: usize,
    t: f32,
    style: EffectStyle,
    rgb: (u8, u8, u8),
    seed: u32,
    edge: Option<Edge>,
    kind: Kind,
) {
    let t = t.clamp(0.0, 1.0);
    let size = size.min(stride);
    if size == 0 {
        return;
    }
    for row in buf.chunks_mut(stride).take(size) {
        row[..size].fill(0);
    }
    if style == EffectStyle::Particles {
        particles(buf, stride, size, t, rgb, seed, edge);
        return;
    }
    let frame = Soft::new(style, t, rgb, seed, edge, kind);
    if size < HALF_RES_FROM {
        shade_parallel(buf, stride, size, &frame);
    } else {
        let low = size.div_ceil(2);
        let mut tmp = vec![0u32; low * low];
        shade_parallel(&mut tmp, low, low, &frame);
        upscale(&tmp, low, buf, stride, size);
    }
    if style == EffectStyle::Rift {
        streaks(buf, stride, size, &frame);
    }
}

/// One frame of a soft style, in coordinates relative to the burst radius
/// (so it can be shaded at any resolution).
struct Soft {
    style: EffectStyle,
    t: f32,
    e: f32,
    rgb: (u8, u8, u8),
    accent: (f32, f32, f32),
    seed: u32,
    edge: Option<Edge>,
    kind: Kind,
    /// Unit vector into the screen.
    inward: (f32, f32),
    /// Smoke puffs: (x, y, 1/r², weight), relative to the radius.
    blobs: Vec<(f32, f32, f32, f32)>,
}

impl Soft {
    fn new(style: EffectStyle, t: f32, rgb: (u8, u8, u8), seed: u32, edge: Option<Edge>, kind: Kind) -> Self {
        let inward = edge.map_or((0.0, -0.4), |e| e.inward());
        // gentler than ease_out: the mist keeps drifting until it is gone
        let drift = 1.0 - (1.0 - t).powi(2);
        let blobs = if style == EffectStyle::Smoke {
            (0..PUFFS)
                .map(|i| {
                    let (sx, sy, ex, ey, r0, r1, w) = puff(seed, i, inward);
                    let r = r0 + (r1 - r0) * drift;
                    (sx + (ex - sx) * drift, sy + (ey - sy) * drift, 1.0 / (r * r), w)
                })
                .collect()
        } else {
            Vec::new()
        };
        Self { style, t, e: ease_out(t), rgb, accent: accent(rgb), seed, edge, kind, inward, blobs }
    }

    /// (depth into the screen, position along the edge) of (u, v).
    fn edge_space(&self, u: f32, v: f32) -> (f32, f32) {
        let (ix, iy) = match self.edge {
            Some(e) => e.inward(),
            None => (1.0, 0.0),
        };
        (u * ix + v * iy, -u * iy + v * ix)
    }

    /// Premultiplied pixel at (u, v), relative to the centre in units of the
    /// radius; `px_per_r` is the radius in pixels.
    fn pixel(&self, u: f32, v: f32, px_per_r: f32) -> u32 {
        let d = (u * u + v * v).sqrt();
        if d >= 1.0 {
            return 0;
        }
        let edge_cut = 1.0 - smoothstep(0.8, 1.0, d);
        let mut px = Px::default();
        match self.style {
            EffectStyle::Smoke => self.smoke(&mut px, u, v, px_per_r),
            EffectStyle::Rift => self.rift(&mut px, u, v, d, px_per_r),
            _ => self.glow(&mut px, d),
        }
        px.r *= edge_cut;
        px.g *= edge_cut;
        px.b *= edge_cut;
        px.a *= edge_cut;
        px.pack()
    }

    fn smoke(&self, px: &mut Px, u: f32, v: f32, px_per_r: f32) {
        let t = self.t;
        let fade_in = smoothstep(0.0, 0.08, t);
        // light along the edge: a thin core and a soft halo that run out
        // along the edge and dim
        if self.edge.is_some() {
            let (across, along) = self.edge_space(u, v);
            let glow = (1.0 - t).powi(2) * fade_in;
            let core_w = 0.045_f32.max(1.5 / px_per_r);
            let len = 0.4 + 0.35 * self.e;
            let l = (-(along / len).powi(2)).exp();
            let a = ((-(across / core_w).powi(2)).exp() * (0.9 - 0.3 * t) + (-(across / 0.2).powi(2)).exp() * 0.45)
                * l
                * glow;
            px.over(a, mix_white(self.rgb, 0.85));
        }
        // mist: soft puffs drifting into the screen, broken up by noise
        let mut density = 0.0;
        for &(bx, by, inv_r2, w) in &self.blobs {
            let k = ((u - bx) * (u - bx) + (v - by) * (v - by)) * inv_r2;
            if k < 6.0 {
                density += w * (-k).exp();
            }
        }
        if density < 0.01 {
            return;
        }
        let nx = u / 0.11 - self.inward.0 * t * 1.6;
        let ny = v / 0.11 - self.inward.1 * t * 1.6 + t;
        let n = 0.85 * fbm2(nx, ny, self.seed) + 0.15 * value_noise(nx * 4.3, ny * 4.3, self.seed ^ 0x1234);
        // dissolve: the thin parts vanish first, so the mist breaks up into
        // wisps instead of fading as one blob
        let erode = 0.05 + 0.45 * smoothstep(0.25, 1.0, t);
        let wisp = smoothstep(erode, erode + 0.5, n * (0.8 + 0.3 * density.min(1.0)));
        let fade = (1.0 - t).powf(1.2) * fade_in;
        let a = ((1.0 - (-density * 2.4).exp()) * (0.4 + 0.7 * wisp)).min(1.0) * fade * 0.95;
        // denser parts are whiter, thin parts slightly grey-blue
        let shade = 0.68 + 0.32 * (density * 0.8).min(1.0);
        let col = mix_white(self.rgb, 0.3);
        px.over(a, (col.0 * shade, col.1 * shade, col.2 * shade));
    }

    fn glow(&self, px: &mut Px, d: f32) {
        let (t, e) = (self.t, self.e);
        let fade = (1.0 - t).powf(1.5) * smoothstep(0.0, 0.05, t);
        let core = bump(d, 0.0, 0.12 + 0.2 * e) * (1.0 - 0.7 * e);
        // main ring and a fainter echo that starts a little later
        let ring = bump(d, 0.2 + 0.7 * e, 0.08 + 0.06 * e) * 0.85;
        let e2 = ease_out(((t - 0.12) / 0.88).max(0.0));
        let echo = bump(d, 0.12 + 0.6 * e2, 0.05 + 0.05 * e2) * 0.4 * smoothstep(0.12, 0.2, t);
        px.over((core + ring + echo) * fade, mix_white(self.rgb, core * 0.5 + ring * 0.15));
    }

    /// A portal tears open on the screen edge: a slit of white light runs
    /// along the edge, a lens of swirling energy with a chromatic rim bulges
    /// into the screen, a shock wave rolls away from it, and at the end it
    /// collapses into a twinkle.
    fn rift(&self, px: &mut Px, u: f32, v: f32, d: f32, px_per_r: f32) {
        let (t, e) = (self.t, self.e);
        let (depth, along) = self.edge_space(u, v);
        let px_w = 1.0 / px_per_r;
        let white = (255.0, 255.0, 255.0);
        let hot = mix_white(self.rgb, 0.75);
        // exit: energy is sucked into the edge; entry: it pours out
        let dir = if self.kind == Kind::Exit { -1.0 } else { 1.0 };

        // bloom: brief soft flash when the rift tears open
        let flash = smoothstep(0.0, 0.05, t) * (1.0 - smoothstep(0.05, 0.4, t));
        px.add(bump(d, 0.0, 0.32) * flash * 0.5, hot);

        // the portal lens
        let open = smoothstep(0.0, 0.16, t) * (1.0 - smoothstep(0.5, 0.92, t));
        let half_len = 0.62 * (0.4 + 0.6 * smoothstep(0.0, 0.22, t)) * (1.0 - 0.55 * smoothstep(0.5, 0.92, t));
        let half_w = (0.23 * open).max(0.004);
        let q = ((depth / half_w).powi(2) + (along / half_len).powi(2)).sqrt();
        // only the half inside the screen (the other half may be a
        // neighbouring display of this machine)
        let inside_screen = if self.edge.is_some() { smoothstep(-0.035, -0.008, depth) } else { 1.0 };
        let open = open * inside_screen;
        if open > 0.002 && q < 1.6 {
            // swirl: rotate faster near the middle, noise flowing in/out
            let ang = dir * (2.6 * (1.0 - q.min(1.0)).powi(2) + t * 4.0);
            let (s, c) = ang.sin_cos();
            let (ra, rb) = (depth * c - along * s, depth * s + along * c);
            let n = fbm2(ra / 0.07, rb / 0.07, self.seed);
            let bands = 0.5 + 0.5 * (q * 13.0 - dir * t * 26.0 + n * 5.0).sin();
            let inside = 1.0 - smoothstep(0.55, 1.0, q);
            let swirl = (n * (0.4 + 0.8 * bands)).min(1.0);
            let energy = inside * (0.4 + 0.55 * swirl) * open;
            let deep = (self.accent.0 * 0.7, self.accent.1 * 0.7, self.accent.2 * 0.85);
            let bright = mix_white(self.rgb, 0.3);
            px.over(energy * 0.85, lerp3(deep, bright, swirl));
            px.add(inside * swirl * swirl * open * 0.35, mix_white(self.rgb, 0.6));
            px.add(bump(q, 0.0, 0.3) * open * 0.4, hot);

            // wobbly chromatic rim: accent outside, white-hot core, colour inside
            let rim_n = fbm2(along / 0.05 + t * 3.0 * dir, depth / 0.05, self.seed ^ 0x77) - 0.5;
            let rq = q - 1.0 + 0.09 * rim_n;
            let w = 0.06_f32.max(1.2 * px_w / half_w.min(half_len));
            px.add(bump(rq, 0.05, w * 1.3) * open * 0.8, self.accent);
            px.add(bump(rq, -0.04, w * 1.2) * open * 0.75, mix_white(self.rgb, 0.15));
            px.add(bump(rq, 0.0, w * 0.4) * open * 0.75, white);
        }

        // slit of light along the edge, running out and dimming
        let slit = smoothstep(0.0, 0.04, t) * (1.0 - t).powf(1.4);
        let len = 0.25 + 0.75 * e;
        let core_w = 0.012_f32.max(1.2 * px_w);
        let lengthwise = bump(along, 0.0, len);
        px.add(bump(depth, 0.0, core_w) * lengthwise * slit * 1.1, white);
        px.add(bump(depth, 0.0, 0.1) * bump(along, 0.0, len * 0.8) * slit * 0.4, hot);

        // shock wave rolling into the screen, with a chromatic split
        let r = 0.1 + 0.88 * e;
        let shock = smoothstep(0.03, 0.1, t) * (1.0 - t).powi(2) * smoothstep(-0.05, 0.02, depth);
        let sw = 0.022_f32.max(1.2 * px_w);
        px.add(bump(d, r, sw) * shock * 0.55, mix_white(self.rgb, 0.5));
        px.add(bump(d, r - 0.03, sw) * shock * 0.35, self.accent);

        // collapse: a four-point star where the rift was
        let star = bump(t, 0.86, 0.07) * (1.0 - smoothstep(0.93, 1.0, t));
        if star > 0.01 {
            let thin = 0.008_f32.max(px_w);
            let rays = bump(depth, 0.0, thin) * bump(along, 0.0, 0.3) + bump(along, 0.0, thin) * bump(depth, 0.0, 0.18);
            px.add((rays + bump(d, 0.0, 0.05) * 0.8) * star, white);
            px.add(bump(d, 0.0, 0.14) * star * 0.35, hot);
        }

        if self.edge.is_some() {
            let k = inside_screen;
            px.r *= k;
            px.g *= k;
            px.b *= k;
            px.a *= k;
        }
    }
}

/// Shades a `size`×`size` square on a few threads (rows are independent).
fn shade_parallel(buf: &mut [u32], stride: usize, size: usize, frame: &Soft) {
    let c = size as f32 / 2.0;
    let shade_rows = |rows: &mut [u32], y0: usize| {
        for (dy, row) in rows.chunks_mut(stride).enumerate() {
            let v = (y0 + dy) as f32 + 0.5 - c;
            for (x, px) in row.iter_mut().take(size).enumerate() {
                let u = x as f32 + 0.5 - c;
                *px = frame.pixel(u / c, v / c, c);
            }
        }
    };
    let area = &mut buf[..stride * (size - 1) + size];
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8);
    if threads == 1 || size < 64 {
        shade_rows(area, 0);
        return;
    }
    let rows_per = size.div_ceil(threads);
    std::thread::scope(|s| {
        for (i, chunk) in area.chunks_mut(stride * rows_per).enumerate() {
            let f = &shade_rows;
            s.spawn(move || f(chunk, i * rows_per));
        }
    });
}

/// Bilinear upscale of a `low`×`low` premultiplied image into the top-left
/// `size`×`size` pixels of `dst`, in integer maths on two channels at a time.
fn upscale(src: &[u32], low: usize, dst: &mut [u32], stride: usize, size: usize) {
    // 8-bit weight w (0..=256) between two packed pixels
    fn lerp(a: u32, b: u32, w: u32) -> u32 {
        let iw = 256 - w;
        let rb = (((a & 0x00FF_00FF) * iw + (b & 0x00FF_00FF) * w) >> 8) & 0x00FF_00FF;
        let ag = (((a >> 8) & 0x00FF_00FF) * iw + ((b >> 8) & 0x00FF_00FF) * w) & 0xFF00_FF00;
        rb | ag
    }
    let k = low as f32 / size as f32;
    let max = (low - 1) as f32;
    // per column: (x0, x1, weight)
    let cols: Vec<(usize, usize, u32)> = (0..size)
        .map(|x| {
            let sx = ((x as f32 + 0.5) * k - 0.5).clamp(0.0, max);
            let x0 = sx as usize;
            (x0, (x0 + 1).min(low - 1), (sx.fract() * 256.0) as u32)
        })
        .collect();
    let scale_rows = |rows: &mut [u32], y_start: usize| {
        for (dy, row) in rows.chunks_mut(stride).enumerate() {
            let sy = (((y_start + dy) as f32 + 0.5) * k - 0.5).clamp(0.0, max);
            let y0 = sy as usize;
            let y1 = (y0 + 1).min(low - 1);
            let wy = (sy.fract() * 256.0) as u32;
            let (r0, r1) = (&src[y0 * low..y0 * low + low], &src[y1 * low..y1 * low + low]);
            for (px, &(x0, x1, wx)) in row.iter_mut().take(size).zip(&cols) {
                let (p00, p01, p10, p11) = (r0[x0], r0[x1], r1[x0], r1[x1]);
                if p00 | p01 | p10 | p11 == 0 {
                    continue;
                }
                *px = lerp(lerp(p00, p01, wx), lerp(p10, p11, wx), wy);
            }
        }
    };
    let area = &mut dst[..stride * (size - 1) + size];
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8);
    if threads == 1 || size < 128 {
        scale_rows(area, 0);
        return;
    }
    let rows_per = size.div_ceil(threads);
    std::thread::scope(|s| {
        for (i, chunk) in area.chunks_mut(stride * rows_per).enumerate() {
            let f = &scale_rows;
            s.spawn(move || f(chunk, i * rows_per));
        }
    });
}

/// Draws a glowing line segment from `tail` to `head` (pixels), brightest at the head.
#[allow(clippy::too_many_arguments)]
fn segment(buf: &mut [u32], stride: usize, size: usize, tail: (f32, f32), head: (f32, f32), width: f32, alpha: f32, head_rgb: (f32, f32, f32), tail_rgb: (f32, f32, f32)) {
    if alpha <= 0.003 {
        return;
    }
    let halo = width * 3.5;
    let x0 = (tail.0.min(head.0) - halo).floor().max(0.0) as usize;
    let x1 = ((tail.0.max(head.0) + halo).ceil().max(0.0) as usize).min(size);
    let y0 = (tail.1.min(head.1) - halo).floor().max(0.0) as usize;
    let y1 = ((tail.1.max(head.1) + halo).ceil().max(0.0) as usize).min(size);
    let (vx, vy) = (head.0 - tail.0, head.1 - tail.1);
    let len2 = (vx * vx + vy * vy).max(1e-4);
    for y in y0..y1 {
        for x in x0..x1 {
            let (px, py) = (x as f32 + 0.5 - tail.0, y as f32 + 0.5 - tail.1);
            let h = ((px * vx + py * vy) / len2).clamp(0.0, 1.0);
            let (dx, dy) = (px - vx * h, py - vy * h);
            let dist = (dx * dx + dy * dy).sqrt();
            let taper = h * h;
            let solid = (width * (0.4 + 0.6 * h) + 0.5 - dist).clamp(0.0, 1.0);
            let glow = (-(dist / halo).powi(2) * 3.0).exp() * 0.5;
            let a = solid.max(glow) * taper * alpha;
            if a > 0.003 {
                let p = &mut buf[y * stride + x];
                let mut q = Px::unpack(*p);
                q.add(a, lerp3(tail_rgb, head_rgb, h));
                *p = q.pack();
            }
        }
    }
}

/// Rift: streaks of light sucked into the edge (exit) or pouring out of it (entry).
fn streaks(buf: &mut [u32], stride: usize, size: usize, f: &Soft) {
    let c = size as f32 / 2.0;
    let (ix, iy) = match f.edge {
        Some(e) => e.inward(),
        None => (1.0, 0.0),
    };
    // without an edge the streaks go all the way round
    let cone = if f.edge.is_some() { 1.25 } else { std::f32::consts::PI };
    let base = iy.atan2(ix);
    let width = (c / 120.0).clamp(0.8, 2.6);
    let head = mix_white(f.rgb, 0.85);
    for i in 0..STREAKS {
        let ang = base + (rnd(f.seed, i, 21) * 2.0 - 1.0) * cone;
        let (dx, dy) = (ang.cos(), ang.sin());
        let reach = 0.5 + 0.45 * rnd(f.seed, i, 22);
        let delay = 0.3 * rnd(f.seed, i, 23);
        let life = 0.45 + 0.25 * rnd(f.seed, i, 24);
        let tau = (f.t - delay) / life;
        if !(0.0..1.0).contains(&tau) {
            continue;
        }
        // where along the slit the streak touches the edge
        let off = (rnd(f.seed, i, 25) * 2.0 - 1.0) * 0.35;
        let origin = (c + -iy * off * c, c + ix * off * c);
        let pos = |tau: f32| -> f32 {
            let tau = tau.clamp(0.0, 1.0);
            match f.kind {
                Kind::Entry => reach * ease_out(tau),
                // accelerate towards the edge
                Kind::Exit => reach * (1.0 - tau * tau),
            }
        };
        let tail_tau = tau - 0.22;
        let at = |r: f32| (origin.0 + dx * r * c, origin.1 + dy * r * c);
        let alpha = (std::f32::consts::PI * tau).sin().powf(0.7) * (0.55 + 0.45 * rnd(f.seed, i, 26));
        let tint = if rnd(f.seed, i, 27) < 0.3 { f.accent } else { mix_white(f.rgb, 0.2) };
        segment(buf, stride, size, at(pos(tail_tau)), at(pos(tau)), width, alpha, head, tint);
    }
}

/// Sparks flying out of the exit point – into the screen when the edge is
/// known – slowing down, shrinking and twinkling as they fade.
fn particles(buf: &mut [u32], stride: usize, size: usize, t: f32, rgb: (u8, u8, u8), seed: u32, edge: Option<Edge>) {
    let c = size as f32 / 2.0;
    let big_r = c;
    let e = ease_out(t);
    let fade = (1.0 - t).powf(1.5) * smoothstep(0.0, 0.04, t);
    // short central flash
    let flash = (1.0 - t).powi(3);
    let flash_r = big_r * 0.25;
    let fr = (flash_r * 2.5) as usize;
    let (lo, hi) = ((c as usize).saturating_sub(fr), (c as usize + fr).min(size));
    for y in lo..hi {
        let dy = y as f32 + 0.5 - c;
        for x in lo..hi {
            let dx = x as f32 + 0.5 - c;
            let d2 = dx * dx + dy * dy;
            let a = (-d2 / (flash_r * flash_r)).exp() * flash * 0.9;
            let px = &mut buf[y * stride + x];
            *px = over(*px, a, mix_white(rgb, 0.4));
        }
    }
    let scale = big_r / 120.0;
    let inward = edge.map(Edge::inward);
    for i in 0..PARTICLES {
        let (mut dx, mut dy, speed, sz) = particle(seed, i);
        // sparks heading off the screen would be invisible: mirror them inwards
        if let Some((ix, iy)) = inward {
            let dot = dx * ix + dy * iy;
            if dot < 0.0 {
                dx -= 2.0 * dot * ix;
                dy -= 2.0 * dot * iy;
            }
        }
        let dist = big_r * 0.9 * speed * e;
        let (px, py) = (c + dx * dist, c + dy * dist);
        let rad = (1.5 + 2.0 * sz) * scale.max(0.5) * (1.0 - 0.45 * t);
        let halo = rad * 2.4;
        let twinkle = 0.75 + 0.25 * (t * 30.0 + rnd(seed, i, 5) * std::f32::consts::TAU).sin();
        let alpha = fade * (0.55 + 0.45 * rnd(seed, i, 4)) * twinkle;
        let x0 = (px - halo).floor().max(0.0) as usize;
        let x1 = ((px + halo).ceil().max(0.0) as usize).min(size);
        let y0 = (py - halo).floor().max(0.0) as usize;
        let y1 = ((py + halo).ceil().max(0.0) as usize).min(size);
        for y in y0..y1 {
            for x in x0..x1 {
                let ddx = x as f32 + 0.5 - px;
                let ddy = y as f32 + 0.5 - py;
                let d = (ddx * ddx + ddy * ddy).sqrt();
                let solid = (rad + 0.5 - d).clamp(0.0, 1.0);
                let glow = (-(d / halo).powi(2) * 3.0).exp() * 0.45;
                let a = solid.max(glow) * alpha;
                let p = &mut buf[y * stride + x];
                *p = over(*p, a, mix_white(rgb, solid * 0.35));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(buf: &[u32], stride: usize, x: usize, y: usize) -> u32 {
        buf[y * stride + x] >> 24
    }

    fn premultiplied(buf: &[u32]) -> bool {
        buf.iter().all(|&p| {
            let a = p >> 24;
            ((p >> 16) & 0xff) <= a && ((p >> 8) & 0xff) <= a && (p & 0xff) <= a
        })
    }

    #[test]
    fn glow_is_bright_in_the_middle_and_fades_out() {
        let (stride, size) = (128, 100);
        let mut buf = vec![0xdead_beef; stride * size];
        render(&mut buf, stride, size, 0.1, EffectStyle::Glow, (56, 189, 248), 1, None, Kind::Exit);
        assert!(alpha_at(&buf, stride, 50, 50) > 150);
        assert_eq!(alpha_at(&buf, stride, 0, 0), 0);
        // outside the square is untouched
        assert_eq!(buf[size], 0xdead_beef);
        assert!(buf.chunks(stride).take(size).all(|r| premultiplied(&r[..size])));
        render(&mut buf, stride, size, 1.0, EffectStyle::Glow, (56, 189, 248), 1, None, Kind::Exit);
        assert!(buf.chunks(stride).take(size).all(|r| r[..size].iter().all(|p| p >> 24 == 0)));
    }

    #[test]
    fn particles_move_outwards() {
        let (stride, size) = (100, 100);
        let mut early = vec![0; stride * size];
        let mut late = vec![0; stride * size];
        render(&mut early, stride, size, 0.05, EffectStyle::Particles, (255, 0, 0), 7, None, Kind::Exit);
        render(&mut late, stride, size, 0.6, EffectStyle::Particles, (255, 0, 0), 7, None, Kind::Exit);
        let spread = |b: &[u32]| {
            let mut sum = 0.0;
            let mut n = 0.0;
            for y in 0..size {
                for x in 0..size {
                    let a = (b[y * stride + x] >> 24) as f32;
                    if a > 20.0 {
                        sum += ((x as f32 - 50.0).powi(2) + (y as f32 - 50.0).powi(2)).sqrt() * a;
                        n += a;
                    }
                }
            }
            sum / n.max(1.0)
        };
        assert!(spread(&late) > spread(&early) + 5.0);
    }

    #[test]
    fn smoke_lights_the_edge_and_drifts_inward() {
        let (stride, size) = (120, 120);
        let mut buf = vec![0; stride * size];
        // right screen edge: light along x = 60, mist drifts left (into the screen)
        render(&mut buf, stride, size, 0.25, EffectStyle::Smoke, (242, 246, 255), 3, Some(Edge::Right), Kind::Exit);
        assert!(alpha_at(&buf, stride, 60, 25) > 60, "edge light above the centre");
        let left: u32 = (0..size).map(|y| alpha_at(&buf, stride, 35, y)).sum();
        let right: u32 = (0..size).map(|y| alpha_at(&buf, stride, 85, y)).sum();
        assert!(left > right, "mist drifts into the screen: {left} vs {right}");
        render(&mut buf, stride, size, 1.0, EffectStyle::Smoke, (242, 246, 255), 3, Some(Edge::Right), Kind::Exit);
        assert!(buf.iter().all(|p| p >> 24 == 0));
    }

    #[test]
    fn rift_opens_on_the_edge_and_bulges_into_the_screen() {
        // big enough for the half-resolution path
        let size = 300;
        let mut buf = vec![0; size * size];
        render(&mut buf, size, size, 0.3, EffectStyle::Rift, (56, 189, 248), 5, Some(Edge::Right), Kind::Exit);
        assert!(premultiplied(&buf));
        // the slit runs along the edge (x = 150)
        assert!(alpha_at(&buf, size, 149, 110) > 120, "slit above the centre");
        // more light inside the screen (left of the edge) than outside it
        let side = |x0: usize, x1: usize| -> u64 {
            (0..size).flat_map(|y| (x0..x1).map(move |x| (x, y))).map(|(x, y)| u64::from(alpha_at(&buf, size, x, y))).sum()
        };
        assert!(side(60, 140) > side(160, 240) * 2, "the portal bulges into the screen");
        render(&mut buf, size, size, 1.0, EffectStyle::Rift, (56, 189, 248), 5, Some(Edge::Right), Kind::Entry);
        assert!(buf.iter().all(|p| p >> 24 == 0));
    }

    #[test]
    fn accent_differs_from_the_colour() {
        let a = accent((56, 189, 248));
        assert!((a.0 - 56.0).abs() + (a.1 - 189.0).abs() + (a.2 - 248.0).abs() > 100.0);
        assert_eq!(accent((242, 246, 255)), (170.0, 120.0, 255.0));
    }
}
