//! Software renderer for one frame of a burst (Windows overlay).
//! Output: premultiplied BGRA pixels (`0xAARRGGBB` as u32), ready for
//! `UpdateLayeredWindow`.

use ddc_core::config::EffectStyle;

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

/// Premultiplied "over": src onto dst.
fn over(dst: u32, a: f32, rgb: (f32, f32, f32)) -> u32 {
    let a = a.clamp(0.0, 1.0);
    if a <= 0.002 {
        return dst;
    }
    let da = (dst >> 24) as f32 / 255.0;
    let dr = ((dst >> 16) & 0xff) as f32;
    let dg = ((dst >> 8) & 0xff) as f32;
    let db = (dst & 0xff) as f32;
    let k = 1.0 - a;
    let oa = a + da * k;
    let or = rgb.0 * a + dr * k;
    let og = rgb.1 * a + dg * k;
    let ob = rgb.2 * a + db * k;
    let c = |v: f32| v.round().clamp(0.0, 255.0) as u32;
    (c(oa * 255.0) << 24) | (c(or) << 16) | (c(og) << 8) | c(ob)
}

fn mix_white(rgb: (u8, u8, u8), w: f32) -> (f32, f32, f32) {
    let m = |v: u8| v as f32 + (255.0 - v as f32) * w.clamp(0.0, 1.0);
    (m(rgb.0), m(rgb.1), m(rgb.2))
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
    edge: Option<crate::Edge>,
) {
    let t = t.clamp(0.0, 1.0);
    for row in buf.chunks_mut(stride).take(size) {
        row[..size].fill(0);
    }
    let c = size as f32 / 2.0;
    let big_r = c;
    let e = ease_out(t);
    let fade = (1.0 - t).powf(1.5);

    match style {
        EffectStyle::Smoke => {
            let fade_in = smoothstep(0.0, 0.08, t);
            // light along the edge: thin bright core + wide soft halo
            if let Some(edge) = edge {
                let glow = (1.0 - t).powi(2) * fade_in;
                let (core_w, halo_w, len) = (big_r * 0.05, big_r * 0.2, big_r * 0.55);
                for y in 0..size {
                    let dy = y as f32 + 0.5 - c;
                    let row = &mut buf[y * stride..y * stride + size];
                    for (x, px) in row.iter_mut().enumerate() {
                        let dx = x as f32 + 0.5 - c;
                        let (across, along) = if edge.vertical() { (dx, dy) } else { (dy, dx) };
                        let l = (-(along / len).powi(2)).exp();
                        let a = ((-(across / core_w).powi(2)).exp() * 0.85
                            + (-(across / halo_w).powi(2)).exp() * 0.45)
                            * l
                            * glow;
                        *px = over(*px, a, mix_white(rgb, 0.85));
                    }
                }
            }
            // mist: soft puffs drifting into the screen, broken up by noise
            let inward = edge.map_or((0.0, -0.4), |e| e.inward());
            let puffs: Vec<_> = (0..PUFFS).map(|i| puff(seed, i, inward)).collect();
            let blobs: Vec<(f32, f32, f32, f32)> = puffs
                .iter()
                .map(|&(sx, sy, ex, ey, r0, r1, w)| {
                    let px = c + (sx + (ex - sx) * e) * big_r;
                    let py = c + (sy + (ey - sy) * e) * big_r;
                    let r = (r0 + (r1 - r0) * e) * big_r;
                    (px, py, 1.0 / (r * r), w)
                })
                .collect();
            let noise_scale = 1.0 / (big_r * 0.11);
            let (drift_x, drift_y) = (inward.0 * t * 1.2, inward.1 * t * 1.2 - t * 0.8);
            let smoke_fade = fade * fade_in;
            for y in 0..size {
                let fy = y as f32 + 0.5;
                let row = &mut buf[y * stride..y * stride + size];
                for (x, px) in row.iter_mut().enumerate() {
                    let fx = x as f32 + 0.5;
                    let mut density = 0.0;
                    for &(bx, by, inv_r2, w) in &blobs {
                        let d2 = (fx - bx) * (fx - bx) + (fy - by) * (fy - by);
                        let k = d2 * inv_r2;
                        if k < 6.0 {
                            density += w * (-k).exp();
                        }
                    }
                    if density < 0.01 {
                        continue;
                    }
                    let nx = fx * noise_scale - drift_x;
                    let ny = fy * noise_scale - drift_y;
                    // rotated octaves hide the axis-aligned value-noise grid
                    let (ax, ay) = (0.866 * nx - 0.5 * ny, 0.5 * nx + 0.866 * ny);
                    let (bx, by) = (0.342 * nx + 0.94 * ny, -0.94 * nx + 0.342 * ny);
                    let n = 0.55 * value_noise(ax, ay, seed)
                        + 0.3 * value_noise(bx * 2.1, by * 2.1, seed ^ 0xABCD)
                        + 0.15 * value_noise(ax * 4.3, ay * 4.3, seed ^ 0x1234);
                    let wisp = 0.3 + 0.85 * smoothstep(0.15, 0.85, n);
                    let dc = ((fx - c).powi(2) + (fy - c).powi(2)).sqrt();
                    let edge_cut = 1.0 - smoothstep(0.8 * big_r, big_r, dc);
                    let a = ((1.0 - (-density * 2.4).exp()) * wisp).min(1.0) * smoke_fade * edge_cut * 0.95;
                    // denser parts are whiter, thin parts slightly grey-blue
                    let shade = 0.68 + 0.32 * (density * 0.8).min(1.0);
                    let col = mix_white(rgb, 0.3);
                    *px = over(*px, a, (col.0 * shade, col.1 * shade, col.2 * shade));
                }
            }
        }
        EffectStyle::Glow => {
            let r = big_r * (0.2 + 0.7 * e);
            let ring_w = big_r * (0.08 + 0.06 * e);
            for y in 0..size {
                let row = &mut buf[y * stride..y * stride + size];
                let dy = y as f32 + 0.5 - c;
                for (x, px) in row.iter_mut().enumerate() {
                    let dx = x as f32 + 0.5 - c;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d >= big_r {
                        continue;
                    }
                    let core = (-(d / (0.45 * r)).powi(2)).exp() * (1.0 - 0.6 * e);
                    let ring = (-((d - r) / ring_w).powi(2)).exp() * 0.85;
                    let edge = 1.0 - smoothstep(0.8 * big_r, big_r, d);
                    let a = (core + ring) * fade * edge;
                    *px = over(*px, a, mix_white(rgb, core * 0.5));
                }
            }
        }
        EffectStyle::Particles => {
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
            for i in 0..PARTICLES {
                let (dx, dy, speed, sz) = particle(seed, i);
                let dist = big_r * 0.9 * speed * e;
                let (px, py) = (c + dx * dist, c + dy * dist);
                let rad = (1.5 + 2.0 * sz) * scale.max(0.5);
                let halo = rad * 2.4;
                let alpha = fade * (0.55 + 0.45 * rnd(seed, i, 4));
                let x0 = (px - halo).floor().max(0.0) as usize;
                let x1 = ((px + halo).ceil() as usize).min(size);
                let y0 = (py - halo).floor().max(0.0) as usize;
                let y1 = ((py + halo).ceil() as usize).min(size);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(buf: &[u32], stride: usize, x: usize, y: usize) -> u32 {
        buf[y * stride + x] >> 24
    }

    #[test]
    fn glow_is_bright_in_the_middle_and_fades_out() {
        let (stride, size) = (128, 100);
        let mut buf = vec![0xdead_beef; stride * size];
        render(&mut buf, stride, size, 0.1, EffectStyle::Glow, (56, 189, 248), 1, None);
        assert!(alpha_at(&buf, stride, 50, 50) > 150);
        assert_eq!(alpha_at(&buf, stride, 0, 0), 0);
        // outside the square is untouched
        assert_eq!(buf[size], 0xdead_beef);
        // premultiplied: colour never exceeds alpha
        for &p in &buf[..size] {
            assert!(((p >> 16) & 0xff) <= p >> 24);
        }
        render(&mut buf, stride, size, 1.0, EffectStyle::Glow, (56, 189, 248), 1, None);
        assert!(buf.chunks(stride).take(size).all(|r| r[..size].iter().all(|p| p >> 24 == 0)));
    }

    #[test]
    fn particles_move_outwards() {
        let (stride, size) = (100, 100);
        let mut early = vec![0; stride * size];
        let mut late = vec![0; stride * size];
        render(&mut early, stride, size, 0.05, EffectStyle::Particles, (255, 0, 0), 7, None);
        render(&mut late, stride, size, 0.6, EffectStyle::Particles, (255, 0, 0), 7, None);
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
        render(&mut buf, stride, size, 0.25, EffectStyle::Smoke, (242, 246, 255), 3, Some(crate::Edge::Right));
        assert!(alpha_at(&buf, stride, 60, 25) > 60, "edge light above the centre");
        let left: u32 = (0..size).map(|y| alpha_at(&buf, stride, 35, y)).sum();
        let right: u32 = (0..size).map(|y| alpha_at(&buf, stride, 85, y)).sum();
        assert!(left > right, "mist drifts into the screen: {left} vs {right}");
        render(&mut buf, stride, size, 1.0, EffectStyle::Smoke, (242, 246, 255), 3, Some(crate::Edge::Right));
        assert!(buf.iter().all(|p| p >> 24 == 0));
    }
}
