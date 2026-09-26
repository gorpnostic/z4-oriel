//! Colour themes. A theme sets a handful of colours; background and body text come from the terminal itself
//! (Color::Reset), so oriel looks native in any terminal. Three kinds:
//!   * built-in palettes (ported from nest)
//!   * "terminal": ANSI colours only, so it follows whatever theme the terminal has
//!   * "omarchy": read from ~/.config/omarchy/current/theme and live-reloaded when you switch Omarchy themes

use ratatui::style::Color;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub bg: Color,     // Reset = the terminal's own background
    pub fg: Color,     // body text
    pub accent: Color, // focused frames, titles, highlights
    pub shine: Color,  // secondary highlight
    pub frame: Color,  // unfocused borders
    pub muted: Color,  // hints, metadata
    pub user: Color,   // user message boxes
    pub inline: Color, // inline code
    pub danger: Color,
    pub good: Color,
    pub animated: bool, // rainbow logo (ultra)
}

// name, accent, shine, frame, muted, user, inline, good, danger. good/danger are each theme's own "added" and
// "removed" (diffs, success and error dots), picked to sit with its accent rather than one green and red for all.
const PALETTES: &[(&str, &str, &str, &str, &str, &str, &str, &str, &str)] = &[
    ("oriel", "#d4884a", "#ffd2a8", "#3c3c3c", "#6e6e6e", "#555555", "#e6b673", "#9cc46a", "#e0694a"),
    ("ember", "#ff5f3a", "#ffc7a8", "#4a2a24", "#7a5c55", "#6a3a30", "#ff9b6b", "#c2cc5a", "#ff4a3a"),
    ("ocean", "#4aa8d4", "#b8e6ff", "#24384a", "#5c6e7a", "#35556a", "#7fd0ff", "#4fd6b0", "#ff7a8a"),
    ("forest", "#6fbf73", "#c8f5c0", "#2a3d2b", "#5e7360", "#3f5a41", "#a6e3a1", "#8fe07a", "#e08a5a"),
    ("sakura", "#f28fb5", "#ffd6e7", "#4a2d3a", "#86697a", "#6a4256", "#ffb3d0", "#8fe0bc", "#ff5f8f"),
    ("synthwave", "#ff4fd8", "#7df9ff", "#3a2360", "#7a6a9a", "#5a3a90", "#7df9ff", "#5af2c0", "#ff4f7b"),
    ("matrix", "#39ff6a", "#c8ffd5", "#12361d", "#3f7a52", "#1f5a30", "#7dff9e", "#39ff6a", "#ff5a4a"),
    ("amber", "#ffb000", "#ffe0a0", "#4a3500", "#8a6a2a", "#6a4c00", "#ffcc55", "#c8d65a", "#ff6a3a"),
    ("dracula", "#bd93f9", "#ffb86c", "#44475a", "#6272a4", "#5a5e7a", "#50fa7b", "#50fa7b", "#ff5555"),
    ("mono", "#e0e0e0", "#ffffff", "#444444", "#777777", "#5a5a5a", "#cfcfcf", "#d8d8d8", "#8c8c8c"),
    ("ultra", "#b48cff", "#8be9fd", "#3d3852", "#7d7896", "#5c5480", "#ff9ad5", "#7dffc8", "#ff6b9d"),
];

pub fn names() -> Vec<String> {
    let mut v: Vec<String> = PALETTES.iter().map(|p| p.0.to_string()).collect();
    v.push("terminal".into());
    if omarchy_dir().is_some() {
        v.insert(0, "omarchy".into());
    }
    v
}

pub fn hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#').trim_start_matches("0x");
    let s = s.get(..6)?;
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

pub fn mix(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
        }
        _ => a,
    }
}

pub fn get(name: &str) -> Theme {
    match name {
        "terminal" => terminal(),
        "omarchy" => omarchy().unwrap_or_else(terminal),
        _ => {
            let p = PALETTES.iter().find(|p| p.0 == name).unwrap_or(&PALETTES[0]);
            let c = |s| hex(s).unwrap_or(Color::Reset);
            Theme {
                name: p.0.into(),
                bg: Color::Reset,
                fg: Color::Reset,
                accent: c(p.1),
                shine: c(p.2),
                frame: c(p.3),
                muted: c(p.4),
                user: c(p.5),
                inline: c(p.6),
                good: c(p.7),
                danger: c(p.8),
                animated: p.0 == "ultra",
            }
        }
    }
}

/// ANSI-only: every colour is one of the terminal's 16, so any terminal theme (Omarchy's included) applies.
fn terminal() -> Theme {
    Theme {
        name: "terminal".into(),
        bg: Color::Reset,
        fg: Color::Reset,
        accent: Color::Blue,
        shine: Color::LightCyan,
        frame: Color::DarkGray,
        muted: Color::DarkGray,
        user: Color::DarkGray,
        inline: Color::Yellow,
        danger: Color::Red,
        good: Color::Green,
        animated: false,
    }
}

/// ~/.config/omarchy/current/theme (a symlink to the active theme's folder), if this is an Omarchy machine.
pub fn omarchy_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let d = home.join(".config/omarchy/current/theme");
    d.exists().then_some(d)
}

/// The folder to watch for theme switches (the symlink itself is replaced, so watch its parent).
pub fn omarchy_watch_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let d = home.join(".config/omarchy/current");
    d.exists().then_some(d)
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// Build a theme from Omarchy's files. colors.toml (newer Omarchy) or alacritty.toml give the palette;
/// hyprland.conf's active border gives the accent, since that's the colour each theme is designed around.
fn omarchy() -> Option<Theme> {
    let dir = omarchy_dir()?;
    let mut pal: std::collections::HashMap<String, Color> = Default::default();
    // colors.toml: flat `accent = "#..."`, `color4 = "#..."`, `background = ...`
    // alacritty.toml: [colors.primary] background/foreground, [colors.normal] black..white, [colors.bright] ...
    for file in ["alacritty.toml", "colors.toml"] {
        if let Ok(v) = read(&dir.join(file)).parse::<toml::Table>() {
            flatten("", &toml::Value::Table(v), &mut pal);
        }
    }
    // hyprland.conf: col.active_border = rgb(89b4fa) rgb(...) 45deg
    let hypr = read(&dir.join("hyprland.conf"));
    let border = hypr
        .lines()
        .find(|l| l.contains("active_border") && !l.contains("inactive"))
        .and_then(|l| {
            let i = l.find("rgb")?;
            let rest = &l[i..];
            let open = rest.find('(')?;
            hex(&rest[open + 1..])
        });
    let g = |keys: &[&str]| keys.iter().find_map(|k| pal.get(*k).copied());
    let bg = g(&["background", "colors.primary.background"])?;
    let fg = g(&["foreground", "colors.primary.foreground"]).unwrap_or(Color::Reset);
    let accent = g(&["accent"]).or(border).or(g(&["color4", "colors.normal.blue"])).unwrap_or(Color::Blue);
    let muted = g(&["color8", "colors.bright.black"]).unwrap_or_else(|| mix(bg, fg, 0.45));
    Some(Theme {
        name: "omarchy".into(),
        bg: Color::Reset, // the terminal is already painted with the theme's background
        fg: Color::Reset,
        accent,
        shine: g(&["color14", "colors.bright.cyan"]).unwrap_or_else(|| mix(accent, fg, 0.5)),
        frame: mix(bg, fg, 0.22),
        muted,
        user: mix(bg, fg, 0.35),
        inline: g(&["color3", "colors.normal.yellow"]).unwrap_or(Color::Yellow),
        danger: g(&["color1", "colors.normal.red"]).unwrap_or(Color::Red),
        good: g(&["color2", "colors.normal.green"]).unwrap_or(Color::Green),
        animated: false,
    })
}

fn flatten(prefix: &str, v: &toml::Value, out: &mut std::collections::HashMap<String, Color>) {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, v, out);
            }
        }
        toml::Value::String(s) => {
            if let Some(c) = hex(s) {
                out.insert(prefix.to_string(), c);
            }
        }
        _ => {}
    }
}

/// Rainbow colour for character i at time t (seconds) — the ultra theme's drifting logo.
pub fn rainbow(i: usize, t: f64) -> Color {
    rainbow_at(i as f64 * 0.07 - t * 0.6, 0.55)
}

/// A point on the rainbow (0..1 wraps) at saturation `s`.
pub fn rainbow_at(pos: f64, s: f64) -> Color {
    let h = pos.rem_euclid(1.0) * 6.0;
    let v = 1.0;
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let f = |u: f64| ((u + m) * 255.0) as u8;
    Color::Rgb(f(r), f(g), f(b))
}
