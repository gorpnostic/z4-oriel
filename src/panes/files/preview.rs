//! Everything in the files app that touches the disk. All of it runs on background threads; the pane only
//! ever renders the plain data these functions return.

use super::clock;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

const TEXT_EXT: &[&str] = &["py", "js", "ts", "tsx", "jsx", "json", "md", "txt", "toml", "yaml", "yml", "ini", "cfg", "html", "htm",
    "css", "scss", "c", "h", "cpp", "hpp", "cc", "cs", "java", "rs", "go", "lua", "ps1", "psm1", "bat", "cmd", "sh", "bash", "zsh",
    "fish", "sql", "xml", "csv", "log", "gitignore", "env", "glsl", "frag", "vert", "wgsl", "mjs", "cjs", "gd", "luau", "kt", "kts",
    "gradle", "properties", "lock", "svg", "conf", "rb", "php", "swift", "zig", "dart", "vue", "svelte", "nix", "desktop", "service"];
const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "tga", "tif", "tiff"];
const AUDIO_EXT: &[&str] = &["mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "wma"];
const VIDEO_EXT: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "wmv", "m4v"];
const ARCHIVE_EXT: &[&str] = &["zip", "7z", "rar", "tar", "gz", "xz", "zst", "bz2", "tgz", "iso"];
const DOC_EXT: &[&str] = &["md", "txt", "pdf", "doc", "docx", "rtf", "odt", "log", "csv", "xls", "xlsx", "ppt", "pptx", "epub"];
/// Build/cache folders nest never showed; they count as hidden (`.` shows them).
pub const SKIP: &[&str] = &["node_modules", "__pycache__", ".git", ".venv", "venv", "dist", "build", ".next", ".gradle", "target"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Folder,
    Code,
    Doc,
    Image,
    Audio,
    Video,
    Archive,
    File,
}

pub fn ext(name: &str) -> String {
    match name.rfind('.') {
        Some(0) => name[1..].to_ascii_lowercase(), // .gitignore, .env
        Some(i) => name[i + 1..].to_ascii_lowercase(),
        None => String::new(),
    }
}

pub fn kind_of(name: &str, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Folder;
    }
    let e = ext(name);
    let e = e.as_str();
    if IMAGE_EXT.contains(&e) {
        Kind::Image
    } else if AUDIO_EXT.contains(&e) {
        Kind::Audio
    } else if VIDEO_EXT.contains(&e) {
        Kind::Video
    } else if ARCHIVE_EXT.contains(&e) {
        Kind::Archive
    } else if DOC_EXT.contains(&e) {
        Kind::Doc
    } else if TEXT_EXT.contains(&e) {
        Kind::Code
    } else {
        Kind::File
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub hidden: bool,
    pub kind: Kind,
}

/// Every entry in `dir`, folders first then by name (case-insensitive). `hidden` marks dotfiles, OS-hidden files
/// and SKIP folders; the pane filters on it.
pub fn list_dir(dir: &Path) -> Result<Vec<Entry>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut out = vec![];
    for de in rd.flatten() {
        let name = de.file_name().to_string_lossy().to_string();
        let ft = de.file_type().ok();
        // symlinks/junctions: follow to see whether they're folders
        let meta = if ft.map(|t| t.is_symlink()).unwrap_or(false) { std::fs::metadata(de.path()).ok() } else { de.metadata().ok() };
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| if m.is_dir() { 0 } else { m.len() }).unwrap_or(0);
        let hidden = name.starts_with('.') || SKIP.contains(&name.as_str());
        #[cfg(windows)]
        let hidden = hidden || {
            use std::os::windows::fs::MetadataExt;
            de.metadata().map(|m| m.file_attributes() & 0x2 != 0).unwrap_or(false) // FILE_ATTRIBUTE_HIDDEN
        };
        out.push(Entry { kind: kind_of(&name, is_dir), name, is_dir, size, hidden });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

// ------------------------------------------------------------------ previews

/// Token classes for the cheap syntax colouring; the pane maps them to theme colours.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Plain,
    Kw,
    Str,
    Com,
    Num,
    Func,
    Head,
    Bold,
}

pub type TLine = Vec<(Tok, String)>;

pub enum Body {
    Folder { readme: Option<(String, Vec<TLine>)>, names: Vec<(bool, String)> },
    /// `numbered` = code (line numbers, no wrap); otherwise prose (wrapped).
    Text { lines: Vec<TLine>, numbered: bool, more: bool },
    Image { w: u32, h: u32, rows: Vec<Vec<([u8; 3], [u8; 3])>> },
    Binary,
    Error(String),
}

pub struct Preview {
    pub name: String,
    pub meta: String,
    pub body: Body,
}

const MAX_LINES: usize = 400;

pub fn build(path: &Path, cols: u16, rows: u16) -> Preview {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string());
    let mk = |meta: String, body: Body| Preview { name: name.clone(), meta, body };
    let st = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return mk(String::new(), Body::Error(e.to_string())),
    };
    let modified = st.modified().map(|t| clock::stamp(clock::secs_of(t))).unwrap_or_default();
    if st.is_dir() {
        let rd = match std::fs::read_dir(path) {
            Ok(r) => r,
            Err(e) => return mk(format!("modified {modified}"), Body::Error(e.to_string())),
        };
        let (mut dirs, mut files) = (vec![], vec![]);
        for de in rd.flatten() {
            let n = de.file_name().to_string_lossy().to_string();
            let is_dir = std::fs::metadata(de.path()).map(|m| m.is_dir()).unwrap_or(false);
            if is_dir { dirs.push(n) } else { files.push(n) }
        }
        let meta = format!("{} {} · {} {} · modified {modified}", dirs.len(), plural(dirs.len(), "folder", "folders"), files.len(), plural(files.len(), "file", "files"));
        let mut readme = None;
        for cand in ["README.md", "readme.md", "Readme.md", "README.txt", "README", "readme.txt"] {
            if files.iter().any(|f| f == cand) {
                let mut s = String::new();
                if let Ok(f) = std::fs::File::open(path.join(cand)) {
                    let mut buf = vec![];
                    let _ = f.take(6000).read_to_end(&mut buf);
                    s = String::from_utf8_lossy(&buf).to_string();
                }
                let lines: Vec<String> = s.lines().map(clean).collect();
                readme = Some((cand.to_string(), highlight(&lines, "md")));
                break;
            }
        }
        dirs.sort_by_key(|d| d.to_lowercase());
        files.sort_by_key(|d| d.to_lowercase());
        let names = dirs.into_iter().take(30).map(|d| (true, d)).chain(files.into_iter().take(40).map(|f| (false, f))).collect();
        return mk(meta, Body::Folder { readme, names });
    }
    let e = ext(&name);
    let meta = format!("{} · modified {modified}", human(st.len()));
    if IMAGE_EXT.contains(&e.as_str()) {
        return match image_rows(path, st.len(), cols, rows) {
            Ok((w, h, px)) => mk(meta, Body::Image { w, h, rows: px }),
            Err(err) => mk(meta, Body::Error(format!("can't draw this image ({err})"))),
        };
    }
    if TEXT_EXT.contains(&e.as_str()) || (st.len() < 200_000 && looks_text(path)) {
        let f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(err) => return mk(meta, Body::Error(err.to_string())),
        };
        let mut r = std::io::BufReader::new(f.take(512 * 1024));
        let mut lines = vec![];
        let mut buf = vec![];
        let mut more = false;
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if lines.len() == MAX_LINES {
                        more = true;
                        break;
                    }
                    let s = String::from_utf8_lossy(&buf);
                    lines.push(clean(s.trim_end_matches(['\n', '\r'])));
                }
            }
        }
        let prose = matches!(e.as_str(), "md" | "txt" | "log" | "") && !name.starts_with('.');
        return mk(meta, Body::Text { lines: highlight(&lines, &e), numbered: !prose, more });
    }
    mk(meta, Body::Binary)
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// nest's size format: "12 B", "3.4 KB".
pub fn human(n: u64) -> String {
    let mut v = n as f64;
    for u in ["B", "KB", "MB", "GB", "TB"] {
        if v < 1024.0 || u == "TB" {
            return if u == "B" { format!("{v:.0} {u}") } else { format!("{v:.1} {u}") };
        }
        v /= 1024.0;
    }
    unreachable!()
}

/// Tabs to spaces, control characters out, very long lines cut (render cost stays flat).
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(1000));
    for c in s.chars().take(1000) {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn looks_text(path: &Path) -> bool {
    let mut buf = [0u8; 2048];
    match std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => !buf[..n].contains(&0),
        Err(_) => false,
    }
}

fn image_rows(path: &Path, size: u64, cols: u16, rows: u16) -> Result<(u32, u32, Vec<Vec<([u8; 3], [u8; 3])>>), String> {
    if size > 64 * 1024 * 1024 {
        return Err("too big to preview".into());
    }
    let img = image::ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?
        .decode().map_err(|e| e.to_string())?;
    let (w, h) = (img.width().max(1), img.height().max(1));
    // one cell = 1 pixel wide, 2 tall (▀ with fg on top, bg below); cells are about twice as tall as wide
    let mut c = (cols as u32).clamp(4, 120).min(w);
    let mut r = ((c as f32 * h as f32 / w as f32) / 2.0).round().max(1.0) as u32;
    if r > rows.max(2) as u32 {
        r = rows.max(2) as u32;
        c = ((r as f32 * 2.0 * w as f32 / h as f32).round() as u32).max(1);
    }
    let small = img.thumbnail_exact(c, r * 2).to_rgba8();
    // transparent pixels blend onto terminal black
    let px = |x: u32, y: u32| {
        let p = small.get_pixel(x, y).0;
        let a = p[3] as u16;
        let bl = |v: u8| ((v as u16 * a + 12 * (255 - a)) / 255) as u8;
        [bl(p[0]), bl(p[1]), bl(p[2])]
    };
    let out = (0..r).map(|y| (0..c).map(|x| (px(x, 2 * y), px(x, 2 * y + 1))).collect()).collect();
    Ok((w, h, out))
}

// ------------------------------------------------------------------ syntax colouring

const KEYWORDS: &[&str] = &["fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "trait", "match", "if", "else",
    "for", "while", "loop", "return", "break", "continue", "const", "static", "def", "class", "import", "from", "as", "in", "not",
    "and", "or", "is", "None", "True", "False", "null", "true", "false", "self", "Self", "this", "new", "function", "var", "async",
    "await", "try", "except", "catch", "finally", "throw", "raise", "with", "yield", "lambda", "package", "interface", "extends",
    "implements", "public", "private", "protected", "void", "int", "float", "double", "bool", "char", "string", "type", "where",
    "elif", "local", "then", "end", "do", "nil", "export", "default", "switch", "case", "typedef", "namespace", "template",
    "virtual", "override", "unsafe", "dyn", "ref", "move", "crate", "super", "undefined", "func", "go", "defer", "select",
    "chan", "range", "val", "fun", "object", "when", "extern", "unsigned", "auto", "delete", "goto", "sizeof", "echo", "fi",
    "esac", "done", "param", "foreach", "until", "global", "pass", "del", "assert", "of", "instanceof", "typeof", "readonly"];

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    CLike, // // and /* */
    Hash,  // #
    Dash,  // --
    Bat,
    Markup, // <!-- -->
    Md,
    Plain,
}

fn lang_of(ext: &str) -> Lang {
    match ext {
        "rs" | "js" | "ts" | "tsx" | "jsx" | "mjs" | "cjs" | "c" | "h" | "cpp" | "hpp" | "cc" | "cs" | "java" | "go" | "kt" | "kts"
        | "gradle" | "glsl" | "frag" | "vert" | "wgsl" | "css" | "scss" | "json" | "swift" | "zig" | "dart" | "php" | "vue" | "svelte" => Lang::CLike,
        "py" | "sh" | "bash" | "zsh" | "fish" | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "gd" | "ps1" | "psm1" | "rb"
        | "properties" | "gitignore" | "env" | "nix" | "desktop" | "service" | "lock" => Lang::Hash,
        "lua" | "luau" | "sql" => Lang::Dash,
        "bat" | "cmd" => Lang::Bat,
        "html" | "htm" | "xml" | "svg" => Lang::Markup,
        "md" => Lang::Md,
        _ => Lang::Plain,
    }
}

/// Colour lines for the preview. Cheap on purpose: one pass per line, no grammar, a single bit of state for
/// block comments / fenced code.
pub fn highlight(lines: &[String], ext: &str) -> Vec<TLine> {
    let lang = lang_of(ext);
    let mut in_block = false;
    lines
        .iter()
        .map(|l| match lang {
            Lang::Plain => vec![(Tok::Plain, l.clone())],
            Lang::Md => md_line(l, &mut in_block),
            _ => code_line(l, lang, ext == "rs", &mut in_block),
        })
        .collect()
}

fn push(out: &mut TLine, t: Tok, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.0 == t {
            last.1.push_str(s);
            return;
        }
    }
    out.push((t, s.to_string()));
}

fn code_line(l: &str, lang: Lang, rust: bool, in_block: &mut bool) -> TLine {
    let mut out = vec![];
    let c: Vec<char> = l.chars().collect();
    let s = |a: usize, b: usize| c[a..b].iter().collect::<String>();
    let starts = |i: usize, pat: &str| pat.chars().enumerate().all(|(k, p)| c.get(i + k) == Some(&p));
    let (bopen, bclose) = match lang {
        Lang::CLike => ("/*", "*/"),
        Lang::Markup => ("<!--", "-->"),
        _ => ("\u{0}", "\u{0}"),
    };
    if lang == Lang::Bat {
        let t = l.trim_start().to_ascii_lowercase();
        if t.starts_with("::") || t.starts_with("rem ") || t == "rem" {
            return vec![(Tok::Com, l.to_string())];
        }
    }
    let mut i = 0;
    while i < c.len() {
        if *in_block {
            let mut j = i;
            while j < c.len() && !starts(j, bclose) {
                j += 1;
            }
            let end = (j + bclose.chars().count()).min(c.len());
            push(&mut out, Tok::Com, &s(i, end));
            if j < c.len() {
                *in_block = false;
            }
            i = end;
            continue;
        }
        let ch = c[i];
        let prev_ws = i == 0 || c[i - 1].is_whitespace();
        let line_com = match lang {
            Lang::CLike => starts(i, "//"),
            Lang::Hash => ch == '#' && (prev_ws || i == 0),
            Lang::Dash => starts(i, "--"),
            _ => false,
        };
        if line_com {
            push(&mut out, Tok::Com, &s(i, c.len()));
            break;
        }
        if (lang == Lang::CLike || lang == Lang::Markup) && starts(i, bopen) {
            *in_block = true;
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            // an apostrophe inside a word (don't, Rust lifetimes) is not a string
            let word_apos = i > 0 && c[i - 1].is_alphanumeric();
            let lifetime = rust && c.get(i + 2) != Some(&'\'') && c.get(i + 1) != Some(&'\\');
            if ch == '\'' && (word_apos || lifetime) {
                push(&mut out, Tok::Plain, "'");
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < c.len() && c[j] != ch {
                if c[j] == '\\' {
                    j += 1;
                }
                j += 1;
            }
            let end = (j + 1).min(c.len());
            push(&mut out, Tok::Str, &s(i, end));
            i = end;
            continue;
        }
        if ch.is_ascii_digit() && (i == 0 || !(c[i - 1].is_alphanumeric() || c[i - 1] == '_')) {
            let mut j = i;
            while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '.' || c[j] == '_') {
                j += 1;
            }
            push(&mut out, Tok::Num, &s(i, j));
            i = j;
            continue;
        }
        if ch.is_alphabetic() || ch == '_' || (ch == '#' && lang == Lang::CLike) || (ch == '$' && lang == Lang::Hash) || (ch == '<' && lang == Lang::Markup) {
            let mut j = i + 1;
            while j < c.len() && (c[j].is_alphanumeric() || c[j] == '_' || (lang == Lang::Markup && (c[j] == '-' || c[j] == ':'))) {
                j += 1;
            }
            let w = s(i, j);
            let tok = if ch == '#' || ch == '$' || ch == '<' {
                if j > i + 1 { Tok::Kw } else { Tok::Plain }
            } else if KEYWORDS.contains(&w.as_str()) {
                Tok::Kw
            } else if c.get(j) == Some(&'(') || c.get(j) == Some(&'!') && lang == Lang::CLike {
                Tok::Func
            } else {
                Tok::Plain
            };
            push(&mut out, tok, &w);
            i = j;
            continue;
        }
        let mut b = [0u8; 4];
        push(&mut out, Tok::Plain, ch.encode_utf8(&mut b));
        i += 1;
    }
    out
}

fn md_line(l: &str, in_fence: &mut bool) -> TLine {
    let t = l.trim_start();
    if t.starts_with("```") || t.starts_with("~~~") {
        *in_fence = !*in_fence;
        return vec![(Tok::Com, l.to_string())];
    }
    if *in_fence {
        return vec![(Tok::Str, l.to_string())];
    }
    if t.starts_with('#') {
        return vec![(Tok::Head, l.to_string())];
    }
    let mut out = vec![];
    let indent = &l[..l.len() - t.len()];
    let mut rest = t;
    for b in ["- ", "* ", "+ ", "> "] {
        if let Some(r) = t.strip_prefix(b) {
            push(&mut out, Tok::Plain, indent);
            push(&mut out, Tok::Kw, b);
            rest = r;
            break;
        }
    }
    if out.is_empty() {
        push(&mut out, Tok::Plain, indent);
    }
    // inline `code` and **bold**
    let mut s = rest;
    while !s.is_empty() {
        let tick = s.find('`');
        let bold = s.find("**");
        match (tick, bold) {
            (Some(a), b) if b.map(|b| a < b).unwrap_or(true) => {
                if let Some(e) = s[a + 1..].find('`') {
                    push(&mut out, Tok::Plain, &s[..a]);
                    push(&mut out, Tok::Str, &s[a..a + e + 2]);
                    s = &s[a + e + 2..];
                    continue;
                }
                push(&mut out, Tok::Plain, s);
                break;
            }
            (_, Some(a)) => {
                if let Some(e) = s[a + 2..].find("**") {
                    push(&mut out, Tok::Plain, &s[..a]);
                    push(&mut out, Tok::Bold, &s[a..a + e + 4]);
                    s = &s[a + e + 4..];
                    continue;
                }
                push(&mut out, Tok::Plain, s);
                break;
            }
            _ => {
                push(&mut out, Tok::Plain, s);
                break;
            }
        }
    }
    out
}

// ------------------------------------------------------------------ places

#[derive(Clone, Debug)]
pub struct PlaceItem {
    /// "home" | "desktop" | "downloads" | "documents" | "code" | "drive"
    pub kind: &'static str,
    pub label: String,
    pub path: PathBuf,
}

/// Sidebar shortcuts: home, desktop, downloads, documents, code. Quick.
pub fn places() -> Vec<PlaceItem> {
    let mut out = vec![];
    if let Some(h) = dirs::home_dir() {
        out.push(PlaceItem { kind: "home", label: "home".into(), path: h });
    }
    for (kind, p) in [("desktop", dirs::desktop_dir()), ("downloads", dirs::download_dir()), ("documents", dirs::document_dir())] {
        if let Some(p) = p.filter(|p| p.is_dir()) {
            out.push(PlaceItem { kind, label: kind.into(), path: p });
        }
    }
    let code = if cfg!(windows) { PathBuf::from(r"C:\Code") } else { dirs::home_dir().map(|h| h.join("Code")).unwrap_or_default() };
    if code.is_dir() {
        out.push(PlaceItem { kind: "code", label: "code".into(), path: code });
    }
    out
}

/// Drives (Windows) or / and mounted disks (Linux). Can be slow (a sleeping network drive): background only.
pub fn drives() -> Vec<PlaceItem> {
    let mut out = vec![];
    #[cfg(windows)]
    for d in b'A'..=b'Z' {
        let root = format!("{}:\\", d as char);
        if Path::new(&root).exists() {
            out.push(PlaceItem { kind: "drive", label: format!("{}: drive", d as char), path: PathBuf::from(root) });
        }
    }
    #[cfg(not(windows))]
    {
        out.push(PlaceItem { kind: "drive", label: "/".into(), path: PathBuf::from("/") });
        let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        let mut seen = vec![PathBuf::from("/")];
        for l in mounts.lines() {
            let mut f = l.split_whitespace();
            let (Some(dev), Some(mnt), Some(fs)) = (f.next(), f.next(), f.next()) else { continue };
            let mnt = mnt.replace("\\040", " ");
            let skip = ["/boot", "/efi", "/snap", "/var", "/proc", "/sys", "/dev", "/run/user", "/tmp", "/usr", "/opt"];
            if !dev.starts_with("/dev/") || fs == "squashfs" || skip.iter().any(|s| mnt.starts_with(s)) {
                continue;
            }
            let p = PathBuf::from(&mnt);
            if seen.contains(&p) {
                continue;
            }
            seen.push(p.clone());
            let label = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(mnt.clone());
            out.push(PlaceItem { kind: "drive", label, path: p });
        }
    }
    out
}

// ------------------------------------------------------------------ OS helpers

#[cfg(windows)]
fn hide_console(c: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    c.creation_flags(0x08000000) // CREATE_NO_WINDOW
}
#[cfg(not(windows))]
fn hide_console(c: &mut std::process::Command) -> &mut std::process::Command {
    c
}

/// Open with the OS default app (explorer on Windows, xdg-open on Linux). Never blocks on the child.
pub fn open_external(path: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut c = if cfg!(windows) {
        let mut c = Command::new("explorer.exe");
        c.arg(path);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(path);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };
    hide_console(&mut c).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait(); // reap it
    });
    Ok(())
}

/// Put text on the clipboard: clip.exe on Windows (fed UTF-16 so any path survives), wl-copy / xclip / xsel on Linux.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let (prog, args, bytes): (&str, Vec<&str>, Vec<u8>) = if cfg!(windows) {
        let mut b = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        ("clip.exe", vec![], b)
    } else if cfg!(target_os = "macos") {
        ("pbcopy", vec![], text.as_bytes().to_vec())
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() && crate::config::which("wl-copy").is_some() {
        ("wl-copy", vec![], text.as_bytes().to_vec())
    } else if crate::config::which("xclip").is_some() {
        ("xclip", vec!["-selection", "clipboard"], text.as_bytes().to_vec())
    } else if crate::config::which("xsel").is_some() {
        ("xsel", vec!["--clipboard", "--input"], text.as_bytes().to_vec())
    } else {
        return Err("no clipboard tool (install wl-clipboard or xclip)".into());
    };
    let mut c = Command::new(prog);
    c.args(&args);
    hide_console(&mut c).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = c.spawn().map_err(|e| format!("{prog}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
