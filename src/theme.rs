//! Colour themes. A theme sets a handful of colours; background and body text come from the terminal itself
//! (Color::Reset), so oriel looks native in any terminal. Three kinds:
//!   * built-in palettes (ported from nest)
//!   * "terminal": ANSI colours only, so it follows whatever theme the terminal has
//!   * "omarchy": read from ~/.config/omarchy/current/theme and live-reloaded when you switch Omarchy themes
//!   * your own: `themes/<name>.toml` next to config.toml, made in the themes app (or by hand), live-reloaded

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
    for c in custom_names() {
        if !v.contains(&c) {
            v.push(c);
        }
    }
    v
}

/// The themes that come with oriel (a theme file can't replace these; it starts from them with `base`).
pub fn builtin_names() -> Vec<String> {
    let mut v: Vec<String> = PALETTES.iter().map(|p| p.0.to_string()).collect();
    v.push("terminal".into());
    v.push("omarchy".into());
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
    get_depth(name, 0)
}

fn get_depth(name: &str, depth: usize) -> Theme {
    if !builtin_names().iter().any(|b| b == name) {
        if let Some(t) = custom(name, depth) {
            return t;
        }
    }
    builtin(name)
}

fn builtin(name: &str) -> Theme {
    match name {
        "terminal" => terminal(),
        "omarchy" => omarchy().unwrap_or_else(terminal),
        _ => {
            let p = PALETTES.iter().find(|p| p.0 == name).unwrap_or(&PALETTES[0]);
            let c = |s| hex(s).unwrap_or(Color::Reset);
            // borders, hints and secondary text lean grey, so the theme's colour is saved for what matters
            // (the focused pane, selections, code) instead of tinting the whole screen
            let calm = |s, grey: Color, a: f32| mix(c(s), grey, a);
            Theme {
                name: p.0.into(),
                bg: Color::Reset,
                fg: Color::Reset,
                accent: c(p.1),
                shine: c(p.2),
                frame: calm(p.3, Color::Rgb(58, 58, 62), 0.55),
                muted: calm(p.4, Color::Rgb(128, 128, 134), 0.5),
                user: calm(p.5, Color::Rgb(92, 92, 98), 0.45),
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

// ------------------------------------------------------------------ your own themes
//
// A theme file is a few lines of TOML in `themes/<name>.toml` next to config.toml:
//
//     base = "ultra"        # start from any theme; only list what you change
//     accent = "#ff79c6"
//     good = "#50fa7b"
//
// The themes app writes these for you; editing one by hand recolours oriel as you save.

#[cfg(test)]
thread_local! {
    /// Tests keep their theme files in a folder of their own.
    pub static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Where your themes live.
pub fn themes_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(d) = TEST_DIR.with(|d| d.borrow().clone()) {
        return d;
    }
    crate::config::dir().join("themes")
}

fn file_of(name: &str) -> PathBuf {
    themes_dir().join(format!("{name}.toml"))
}

pub fn custom_names() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(themes_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "toml").then(|| p.file_stem().map(|s| s.to_string_lossy().to_string()))?
        })
        .collect();
    v.sort();
    v
}

pub fn is_custom(name: &str) -> bool {
    !builtin_names().iter().any(|b| b == name) && file_of(name).is_file()
}

/// The settings a theme file knows, in the order the themes app lists them: (key, what it colours).
pub const COLOR_KEYS: &[(&str, &str)] = &[
    ("accent", "focused frames, titles, keywords in code"),
    ("shine", "the second highlight: functions in code, links"),
    ("good", "added lines in diffs, success"),
    ("danger", "removed lines, errors"),
    ("inline", "inline code and strings"),
    ("muted", "hints and secondary text"),
    ("frame", "borders of panes you're not in"),
    ("user", "the border of your messages"),
    ("text", "body text (\"terminal\" = your terminal's)"),
    ("background", "the background (\"terminal\" = your terminal's)"),
];
const OTHER_KEYS: &[&str] = &["base", "rainbow", "fg", "bg"];

pub fn field(t: &Theme, key: &str) -> Color {
    match key {
        "accent" => t.accent,
        "shine" => t.shine,
        "good" => t.good,
        "danger" => t.danger,
        "inline" => t.inline,
        "muted" => t.muted,
        "frame" => t.frame,
        "user" => t.user,
        "text" | "fg" => t.fg,
        _ => t.bg,
    }
}

pub fn set_field(t: &mut Theme, key: &str, c: Color) {
    match key {
        "accent" => t.accent = c,
        "shine" => t.shine = c,
        "good" => t.good = c,
        "danger" => t.danger = c,
        "inline" => t.inline = c,
        "muted" => t.muted = c,
        "frame" => t.frame = c,
        "user" => t.user = c,
        "text" | "fg" => t.fg = c,
        "background" | "bg" => t.bg = c,
        _ => {}
    }
}

/// A colour in a theme file: "#rrggbb", "#rgb", an ANSI name ("red", "bright-blue"), or "terminal" for the
/// terminal's own.
pub fn color(s: &str) -> Option<Color> {
    let s = s.trim().to_ascii_lowercase();
    if matches!(s.as_str(), "terminal" | "default" | "none" | "reset") {
        return Some(Color::Reset);
    }
    if let Some(h) = s.strip_prefix('#') {
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        return match h.len() {
            3 => hex(&h.chars().flat_map(|c| [c, c]).collect::<String>()),
            6 => hex(h),
            _ => None,
        };
    }
    Some(match s.replace(['-', '_', ' '], "").as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" | "gray" | "grey" => Color::Gray,
        "brightblack" | "darkgray" | "darkgrey" => Color::DarkGray,
        "brightred" => Color::LightRed,
        "brightgreen" => Color::LightGreen,
        "brightyellow" => Color::LightYellow,
        "brightblue" => Color::LightBlue,
        "brightmagenta" => Color::LightMagenta,
        "brightcyan" => Color::LightCyan,
        "brightwhite" => Color::White,
        _ => return None,
    })
}

/// How a colour is written in a theme file.
pub fn color_name(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Reset => "terminal".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "white".into(),
        Color::DarkGray => "bright-black".into(),
        Color::LightRed => "bright-red".into(),
        Color::LightGreen => "bright-green".into(),
        Color::LightYellow => "bright-yellow".into(),
        Color::LightBlue => "bright-blue".into(),
        Color::LightMagenta => "bright-magenta".into(),
        Color::LightCyan => "bright-cyan".into(),
        Color::White => "bright-white".into(),
        other => {
            let (r, g, b) = approx_rgb(other, (200, 200, 200));
            format!("#{r:02x}{g:02x}{b:02x}")
        }
    }
}

/// An RGB value for any colour (ANSI ones as the usual terminal palette); `fallback` for "the terminal's own".
pub fn approx_rgb(c: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (30, 30, 30),
        Color::Red => (224, 90, 90),
        Color::Green => (111, 191, 115),
        Color::Yellow => (230, 182, 115),
        Color::Blue => (74, 168, 212),
        Color::Magenta => (189, 147, 249),
        Color::Cyan => (86, 200, 216),
        Color::Gray => (207, 207, 207),
        Color::DarkGray => (110, 110, 110),
        Color::LightRed => (255, 123, 114),
        Color::LightGreen => (166, 227, 161),
        Color::LightYellow => (255, 213, 128),
        Color::LightBlue => (127, 208, 255),
        Color::LightMagenta => (214, 172, 255),
        Color::LightCyan => (139, 233, 253),
        Color::White => (255, 255, 255),
        _ => fallback,
    }
}

/// RGB -> (hue 0-360, saturation 0-1, lightness 0-1).
pub fn to_hsl((r, g, b): (u8, u8, u8)) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

pub fn from_hsl(h: f32, s: f32, l: f32) -> Color {
    let (h, s, l) = (h.rem_euclid(360.0) / 360.0, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0));
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return Color::Rgb(v, v, v);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round() as u8
    };
    Color::Rgb(f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

/// The last version of each theme file that parsed, so a typo mid-edit doesn't flash another theme.
static LAST_GOOD: std::sync::Mutex<Vec<(String, Theme)>> = std::sync::Mutex::new(Vec::new());

fn custom(name: &str, depth: usize) -> Option<Theme> {
    let text = std::fs::read_to_string(file_of(name)).ok()?;
    let Ok(tbl) = text.parse::<toml::Table>() else {
        let last = LAST_GOOD.lock().unwrap().iter().find(|(n, _)| n == name).map(|(_, t)| t.clone());
        return Some(last.unwrap_or_else(|| Theme { name: name.to_string(), ..builtin("ultra") }));
    };
    let base = tbl.get("base").and_then(|v| v.as_str()).unwrap_or("ultra");
    let mut t = if base == name || depth >= 4 { builtin(base) } else { get_depth(base, depth + 1) };
    t.name = name.to_string();
    for (k, v) in &tbl {
        if let Some(c) = v.as_str().and_then(color) {
            set_field(&mut t, k, c);
        }
    }
    if let Some(b) = tbl.get("rainbow").and_then(|v| v.as_bool()) {
        t.animated = b;
    }
    let mut last = LAST_GOOD.lock().unwrap();
    last.retain(|(n, _)| n != name);
    last.push((name.to_string(), t.clone()));
    Some(t)
}

/// What a theme file starts from (its `base`), for "reset to the original".
pub fn base_of(name: &str) -> String {
    if !is_custom(name) {
        return name.to_string();
    }
    std::fs::read_to_string(file_of(name)).ok().and_then(|s| s.parse::<toml::Table>().ok()).and_then(|t| t.get("base").and_then(|v| v.as_str()).map(String::from)).unwrap_or_else(|| "ultra".into())
}

/// Anything wrong with a theme file, in words ("mine.toml: 'acent' isn't a setting").
pub fn problems(name: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(file_of(name)) else { return vec![] };
    let file = format!("{name}.toml");
    match text.parse::<toml::Table>() {
        Err(e) => vec![format!("{file}: {}", e.to_string().lines().next().unwrap_or("can't read it"))],
        Ok(tbl) => tbl
            .iter()
            .filter_map(|(k, v)| {
                let known = COLOR_KEYS.iter().any(|(c, _)| c == k) || OTHER_KEYS.contains(&k.as_str());
                if !known {
                    return Some(format!("{file}: '{k}' isn't a setting"));
                }
                match k.as_str() {
                    "base" => v.as_str().filter(|b| !names().iter().any(|n| n == b)).map(|b| format!("{file}: there's no theme called '{b}' to start from")),
                    "rainbow" => (!v.is_bool()).then(|| format!("{file}: rainbow is true or false")),
                    _ => v.as_str().and_then(color).is_none().then(|| format!("{file}: '{k}' isn't a colour (write \"#rrggbb\")")),
                }
            })
            .collect(),
    }
}

/// A name that's free for a new theme: "my-ultra", "my-ultra-2"…
pub fn free_name(want: &str) -> String {
    let clean: String = want.trim().to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    let clean = clean.trim_matches('-').to_string();
    let clean = if clean.is_empty() { "mine".to_string() } else { clean };
    let taken = |n: &str| names().iter().any(|x| x == n) || builtin_names().iter().any(|x| x == n);
    if !taken(&clean) {
        return clean;
    }
    (2..).map(|i| format!("{clean}-{i}")).find(|n| !taken(n)).unwrap()
}

/// Write `t` as the theme file `name` (every colour spelled out, `base` for anything added later).
pub fn save_custom(name: &str, base: &str, t: &Theme) -> Result<PathBuf, String> {
    let dir = themes_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut s = format!(
        "# {name}: an oriel theme. Change a colour and save: oriel recolours as you go (or use the themes app).\n\
         # Colours are \"#rrggbb\", an ANSI name like \"bright-blue\", or \"terminal\" for your terminal's own.\n\
         # To share it, send this file: it goes in the themes folder next to oriel's config.toml.\n\n"
    );
    s.push_str(&format!("{:<26}# starts from this theme; a line you delete keeps its colour\n", format!("base = \"{base}\"")));
    for (k, what) in COLOR_KEYS {
        s.push_str(&format!("{:<26}# {what}\n", format!("{k} = \"{}\"", color_name(field(t, k)))));
    }
    s.push_str(&format!("{:<26}# the animated rainbow logo\n", format!("rainbow = {}", t.animated)));
    let path = file_of(name);
    std::fs::write(&path, s).map_err(|e| e.to_string())?;
    Ok(path)
}
