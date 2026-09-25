//! Cover art, drawn in half-block pixels (each cell a ▀ with fg = top pixel, bg = bottom pixel), and the cover's
//! most vivid colour, which tints the player the way nest and the audio-player app do.

use super::library::{self, Track};
use image::{DynamicImage, imageops::FilterType};
use ratatui::style::Color;

pub struct Cover {
    pub w: u16,
    pub rows: u16,
    /// w * rows*2 pixels, row-major.
    pub px: Vec<[u8; 3]>,
    pub accent: Option<Color>,
}

/// The app's cover override, else a cover.jpg/folder.jpg beside the file, else art embedded in the file.
/// Blocking (file reads + decode): call from a background thread.
pub fn load(t: &Track, w: u32, rows: u32) -> Option<Cover> {
    let img = find(t)?;
    Some(Cover { w: w as u16, rows: rows as u16, px: pixels(&img, w, rows * 2), accent: accent(&img) })
}

fn find(t: &Track) -> Option<DynamicImage> {
    if let Some(dir) = library::app_dir() {
        for ext in ["png", "jpg", "jpeg", "webp"] {
            let p = dir.join("covers").join(format!("{}.{ext}", t.id));
            if let Ok(img) = image::open(&p) {
                return Some(img);
            }
        }
    }
    if let Some(dir) = t.path.parent() {
        for name in ["cover.jpg", "cover.png", "folder.jpg", "folder.png", "front.jpg", "album.jpg"] {
            if let Ok(img) = image::open(dir.join(name)) {
                return Some(img);
            }
        }
    }
    let mut probed = library::probe(&t.path)?;
    let mut data: Option<Vec<u8>> = None;
    if let Some(m) = probed.metadata.get() {
        if let Some(v) = m.current().and_then(|r| r.visuals().first()) {
            data = Some(v.data.to_vec());
        }
    }
    if data.is_none() {
        if let Some(v) = probed.format.metadata().current().and_then(|r| r.visuals().first()) {
            data = Some(v.data.to_vec());
        }
    }
    image::load_from_memory(&data?).ok()
}

/// Centre-crop to a square and scale to w x h.
fn pixels(img: &DynamicImage, w: u32, h: u32) -> Vec<[u8; 3]> {
    let (iw, ih) = (img.width(), img.height());
    let side = iw.min(ih).max(1);
    let sq = img.crop_imm((iw - side) / 2, (ih - side) / 2, side, side);
    let small = sq.resize_exact(w, h, FilterType::Lanczos3).to_rgb8();
    small.pixels().map(|p| p.0).collect()
}

/// nest's cover_accent: the most vivid colour, pushed bright enough to read on a dark terminal.
fn accent(img: &DynamicImage) -> Option<Color> {
    let small = img.resize_exact(24, 24, FilterType::Triangle).to_rgb8();
    let mut best: Option<(f32, f32, f32)> = None;
    let mut score = 0.0f32;
    for p in small.pixels() {
        let [r, g, b] = p.0;
        let (h, s, v) = hsv(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        let sc = s * v * if v > 0.35 { 1.0 } else { 0.2 };
        if sc > score {
            best = Some((h, s, v));
            score = sc;
        }
    }
    let (h, s, v) = best?;
    if score < 0.18 {
        return None;
    }
    let (r, g, b) = rgb(h, s.clamp(0.55, 1.0), v.max(0.8));
    Some(Color::Rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8))
}

fn hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / d).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    (h, if max == 0.0 { 0.0 } else { d / max }, max)
}

fn rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
    match i as i32 % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}
