//! Renders the effect styles into an image for the README:
//! `cargo run -p effect --example preview -- docs/img/effects.bmp`
//! One row per style (rift twice: exit, entry), five frames each, composited over a dark background
//! with the burst on a vertical screen edge (like PbP: left half | right half).

use std::fs;

use ddc_core::config::EffectStyle;
use effect::render::render;
use effect::{Edge, Kind};

const CELL: usize = 280;
const FRAMES: [f32; 5] = [0.06, 0.2, 0.4, 0.62, 0.86];

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "effects.bmp".into());
    let styles = [
        (EffectStyle::Rift, Kind::Exit),
        (EffectStyle::Rift, Kind::Entry),
        (EffectStyle::Smoke, Kind::Exit),
        (EffectStyle::Glow, Kind::Exit),
        (EffectStyle::Particles, Kind::Exit),
    ];
    let (w, h) = (CELL * FRAMES.len(), CELL * styles.len());
    let mut img = vec![[0u8; 3]; w * h];

    for (row, (style, kind)) in styles.iter().enumerate() {
        for (col, t) in FRAMES.iter().enumerate() {
            let mut buf = vec![0u32; CELL * CELL];
            let rgb = match (style, kind) {
                (EffectStyle::Smoke, _) | (EffectStyle::Rift, Kind::Entry) => (0xF2, 0xF6, 0xFF),
                _ => (0x38, 0xBD, 0xF8),
            };
            render(&mut buf, CELL, CELL, *t, *style, rgb, 7, Some(Edge::Right), *kind);
            for y in 0..CELL {
                for x in 0..CELL {
                    // background: two "screens" meeting in the middle of the cell
                    let left = x < CELL / 2;
                    let g = (y as f32 / CELL as f32 * 18.0) as u8;
                    let bg = if left { [22 + g, 26 + g, 38 + g] } else { [30 + g, 20 + g, 34 + g] };
                    let p = buf[y * CELL + x];
                    let a = (p >> 24) as f32 / 255.0;
                    let src = [(p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff];
                    let px = &mut img[(row * CELL + y) * w + col * CELL + x];
                    for c in 0..3 {
                        px[c] = (src[c] as f32 + bg[c] as f32 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }
    if let Err(e) = fs::write(&out, bmp(w, h, &img)) {
        eprintln!("{out}: {e}");
        std::process::exit(1);
    }
    println!("wrote {out} ({w}x{h})");
}

/// 24-bit bottom-up BMP.
fn bmp(w: usize, h: usize, rgb: &[[u8; 3]]) -> Vec<u8> {
    let row = (w * 3).div_ceil(4) * 4;
    let size = 54 + row * h;
    let mut v = Vec::with_capacity(size);
    v.extend_from_slice(b"BM");
    v.extend_from_slice(&(size as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&54u32.to_le_bytes());
    v.extend_from_slice(&40u32.to_le_bytes());
    v.extend_from_slice(&(w as i32).to_le_bytes());
    v.extend_from_slice(&(h as i32).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&24u16.to_le_bytes());
    v.extend_from_slice(&[0u8; 24]);
    for y in (0..h).rev() {
        for x in 0..w {
            let [r, g, b] = rgb[y * w + x];
            v.extend_from_slice(&[b, g, r]);
        }
        v.resize(v.len() + row - w * 3, 0);
    }
    v
}
