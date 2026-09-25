//! Filesystem side of the storage app: link-safe folder sizes, the cleanup list (per OS), drives, and emptying a
//! cache folder. Everything here runs on background threads; the pane never calls it from render.
//!
//! Safety rules (ported from nest):
//!   * the walker never follows symlinks, junctions or any other reparse point. Windows'
//!     `AppData\Local\Application Data` is a junction back to `AppData\Local`; following it double-counts ~150 GB
//!   * on Linux a walk never crosses into another filesystem (like `du -x`), and /proc /sys /dev /run are skipped
//!   * "clean" rows are only caches/temp that programs rebuild; anything that is your data is "review" and only
//!     ever opens in the files app

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// True for symlinks, junctions and every other reparse point (OneDrive placeholders too: reading those would
/// download them). `md` must come from `symlink_metadata` / `DirEntry::metadata`, which don't follow links.
pub fn is_link(md: &Metadata) -> bool {
    if md.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & 0x400 != 0 {
            return true; // FILE_ATTRIBUTE_REPARSE_POINT
        }
    }
    false
}

#[cfg(unix)]
fn dev(md: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.dev()
}
#[cfg(not(unix))]
fn dev(_md: &Metadata) -> u64 {
    0
}

/// Virtual filesystems at the root of a Linux box: never measured.
pub fn skip_root_child(p: &Path) -> bool {
    cfg!(unix) && matches!(p.to_str(), Some("/proc" | "/sys" | "/dev" | "/run"))
}

/// Files bigger than this are remembered as "biggest files in here".
pub const BIG_FILE: u64 = 100 * 1024 * 1024;

/// Bytes under `path` without following links. `progress` gets the running total every ~150 ms, `big` collects
/// files over BIG_FILE. Unreadable entries (in use, no permission) are skipped.
pub fn dir_size(path: &Path, stop: &AtomicBool, mut big: Option<&mut Vec<(u64, PathBuf)>>, progress: &mut dyn FnMut(u64)) -> u64 {
    let Ok(root) = fs::symlink_metadata(path) else { return 0 };
    if is_link(&root) {
        return 0;
    }
    if !root.is_dir() {
        return root.len();
    }
    let home_dev = dev(&root);
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    let mut n = 0u32;
    let mut last = Instant::now();
    while let Some(d) = stack.pop() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if is_link(&md) {
                continue;
            }
            if md.is_dir() {
                if dev(&md) == home_dev {
                    stack.push(e.path());
                }
            } else {
                let sz = md.len();
                total += sz;
                if sz > BIG_FILE {
                    if let Some(b) = big.as_deref_mut() {
                        b.push((sz, e.path()));
                    }
                }
            }
            n += 1;
            if n % 512 == 0 && last.elapsed() > Duration::from_millis(150) {
                last = Instant::now();
                progress(total);
            }
        }
    }
    total
}

/// Refuse anything that looks like a root, a home folder or a drive: cleanup only ever empties deep cache folders.
fn deep_enough(p: &Path) -> bool {
    let home = dirs::home_dir();
    p.is_absolute() && p.components().count() >= 4 && Some(p) != home.as_deref()
}

/// Delete one entry (recursively for real folders). Links are removed themselves, never what they point at.
/// Returns the bytes actually freed; files that are in use just stay.
fn delete_entry(p: &Path, md: &Metadata) -> u64 {
    if is_link(md) {
        let _ = fs::remove_file(p).or_else(|_| fs::remove_dir(p));
        return 0;
    }
    if md.is_dir() {
        let mut freed = 0;
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() {
                if let Ok(m) = e.metadata() {
                    freed += delete_entry(&e.path(), &m);
                }
            }
        }
        let _ = fs::remove_dir(p);
        return freed;
    }
    match fs::remove_file(p) {
        Ok(()) => md.len(),
        Err(_) if md.permissions().readonly() => {
            // read-only cache files (git packs in cargo/yay caches) need the flag cleared first
            let mut perm = md.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            if fs::set_permissions(p, perm).is_ok() && fs::remove_file(p).is_ok() { md.len() } else { 0 }
        }
        Err(_) => 0,
    }
}

/// Empty a cache folder, keeping the folder itself. Only call after the user confirmed.
pub fn clear_dir(path: &Path) -> u64 {
    if !deep_enough(path) {
        return 0;
    }
    let Ok(md) = fs::symlink_metadata(path) else { return 0 };
    if is_link(&md) || !md.is_dir() {
        return 0;
    }
    let mut freed = 0;
    if let Ok(rd) = fs::read_dir(path) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                freed += delete_entry(&e.path(), &m);
            }
        }
    }
    freed
}

// ------------------------------------------------------------------ the cleanup list

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// caches/temp: safe to empty
    Clean,
    /// your data: look first, opens in files
    Review,
}

/// A command line, shown in the confirmation and run in a terminal pane or in the background.
#[derive(Clone, Debug, PartialEq)]
pub struct Cmd {
    pub prog: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new(prog: &str, args: &[&str]) -> Cmd {
        Cmd { prog: prog.into(), args: args.iter().map(|s| s.to_string()).collect() }
    }
    pub fn text(&self) -> String {
        let mut s = self.prog.clone();
        for a in &self.args {
            s.push(' ');
            if a.contains(' ') { s.push_str(&format!("\"{a}\"")) } else { s.push_str(a) }
        }
        s
    }
}

#[derive(Clone, Debug)]
pub enum How {
    /// empty the folders in `paths`
    Dir,
    /// Windows: Clear-RecycleBin
    Recycle,
    /// `pnpm store prune`, in the background
    Pnpm,
    /// needs root / a password: run in a terminal pane so the prompt works
    Term(Cmd),
    /// review rows: never deleted
    Nothing,
}

/// Where a row's paths come from.
#[derive(Clone, Debug)]
pub enum Find {
    Fixed,
    NodeModules(PathBuf),
    ElectronDbs(PathBuf),
    /// ~/.cache minus the folders other rows already count
    CacheOther(PathBuf),
    /// size from `journalctl --disk-usage`
    Journal,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub id: &'static str,
    pub label: String,
    pub paths: Vec<PathBuf>,
    pub kind: Kind,
    pub note: String,
    pub how: How,
    pub find: Find,
    /// what enter opens in files
    pub open: Option<PathBuf>,
}

fn t(id: &'static str, label: &str, paths: Vec<PathBuf>, kind: Kind, note: &str, how: How) -> Target {
    Target { id, label: label.into(), paths, kind, note: note.into(), how, find: Find::Fixed, open: None }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// ~/.cache folders that have a row of their own (so "other app caches" doesn't count them twice).
const CLAIMED_CACHE: &[&str] = &[
    "huggingface", "yay", "paru", "pip", "mesa_shader_cache", "mesa_shader_cache_db", "nvidia", "mozilla",
    "google-chrome", "chromium", "BraveSoftware", "thumbnails",
];

/// The cleanup list for this OS. Touches the filesystem (exists checks): call from a worker.
pub fn targets() -> Vec<Target> {
    let mut v = if cfg!(windows) { windows_targets() } else { linux_targets() };
    let h = home();
    v.push(t("downloads", "Downloads", vec![dirs::download_dir().unwrap_or_else(|| h.join("Downloads"))], Kind::Review,
        "your files — look before deleting", How::Nothing));
    v.push(t("hf", "Hugging Face models", vec![h.join(".cache").join("huggingface")], Kind::Review, "downloaded ML models/datasets", How::Nothing));
    v.push(t("ollama", "Ollama models", vec![h.join(".ollama").join("models"), PathBuf::from("/usr/share/ollama/.ollama/models")],
        Kind::Review, "remove with `ollama rm <name>`", How::Nothing));
    let (apps, code) = if cfg!(windows) {
        (dirs::config_dir().unwrap_or_else(|| h.join("AppData").join("Roaming")), PathBuf::from(r"C:\Code"))
    } else {
        (dirs::config_dir().unwrap_or_else(|| h.join(".config")), h.clone())
    };
    let mut idb = t("idb", "Electron app databases", vec![], Kind::Review, "apps storing media in IndexedDB (video-editor once held 38 GB here)", How::Nothing);
    idb.find = Find::ElectronDbs(apps.clone());
    idb.open = Some(apps);
    v.push(idb);
    let label = if cfg!(windows) { "node_modules in C:\\Code".to_string() } else { "node_modules under ~".to_string() };
    let mut nm = t("nodemods", &label, vec![], Kind::Review, "reinstall any time with pnpm install", How::Nothing);
    nm.find = Find::NodeModules(code.clone());
    nm.open = Some(code);
    v.push(nm);
    if !cfg!(windows) {
        let c = h.join(".cache");
        let mut other = t("cache", "other app caches (~/.cache)", vec![], Kind::Review, "most apps rebuild these, but look first", How::Nothing);
        other.find = Find::CacheOther(c.clone());
        other.open = Some(c);
        v.push(other);
    }
    // keep rows whose folders exist (found-later rows always stay)
    v.into_iter()
        .filter_map(|mut tg| {
            if matches!(tg.find, Find::Fixed) {
                tg.paths.retain(|p| p.is_dir());
                if tg.paths.is_empty() {
                    return None;
                }
            }
            if tg.open.is_none() {
                tg.open = tg.paths.first().cloned();
            }
            Some(tg)
        })
        .collect()
}

fn windows_targets() -> Vec<Target> {
    let h = home();
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| h.join("AppData").join("Local"));
    let temp = std::env::var_os("TEMP").map(PathBuf::from).unwrap_or_else(|| local.join("Temp"));
    let mut v = vec![
        t("temp", "temp files", vec![temp], Kind::Clean, "leftovers from installers and apps; files in use are skipped", How::Dir),
        t("pip", "pip cache", vec![local.join("pip").join("cache")], Kind::Clean, "downloaded wheels; pip re-downloads", How::Dir),
        t("npm", "npm cache", vec![local.join("npm-cache")], Kind::Clean, "npm re-downloads what it needs", How::Dir),
        t("pnpm", "pnpm store (unused)", vec![local.join("pnpm").join("store")], Kind::Clean,
            "only packages no project uses are removed (pnpm store prune)", How::Pnpm),
        t("cargo", "cargo download cache", vec![h.join(".cargo").join("registry").join("cache")], Kind::Clean,
            "downloaded crates; cargo re-downloads", How::Dir),
        t("shaders", "shader caches", vec![local.join("D3DSCache"), local.join("NVIDIA").join("DXCache"), local.join("NVIDIA").join("GLCache")],
            Kind::Clean, "games/GPU rebuild them (first launch may stutter)", How::Dir),
        t("dumps", "crash dumps", vec![local.join("CrashDumps")], Kind::Clean, "memory dumps from crashed apps", How::Dir),
        t("chrome", "browser caches", vec![local.join(r"Google\Chrome\User Data\Default\Cache"), local.join(r"Microsoft\Edge\User Data\Default\Cache")],
            Kind::Clean, "close the browser first; logins and history are kept", How::Dir),
    ];
    if let Some(bin) = recycle_bin_dir() {
        v.insert(1, t("recycle", "recycle bin", vec![bin], Kind::Clean, "things you already deleted", How::Recycle));
    }
    v
}

fn linux_targets() -> Vec<Target> {
    let h = home();
    let c = h.join(".cache");
    let data = dirs::data_dir().unwrap_or_else(|| h.join(".local").join("share"));
    let trash = data.join("Trash");
    let pacman_clean = if crate::config::which("paccache").is_some() {
        Cmd::new("sudo", &["paccache", "-rk1"])
    } else {
        Cmd::new("sudo", &["pacman", "-Sc"])
    };
    let pac_note = if pacman_clean.args[0] == "paccache" {
        "old package versions, keeps the newest (sudo paccache -rk1)"
    } else {
        "packages no longer installed (sudo pacman -Sc)"
    };
    let mut journal = t("journal", "systemd journal", vec![PathBuf::from("/var/log/journal")], Kind::Clean,
        "old system logs, keeps two weeks (sudo journalctl --vacuum-time=2weeks)",
        How::Term(Cmd::new("sudo", &["journalctl", "--vacuum-time=2weeks"])));
    journal.find = Find::Journal;
    journal.open = Some(PathBuf::from("/var/log/journal"));
    let mut v = vec![
        t("trash", "trash", vec![trash.join("files"), trash.join("info")], Kind::Clean, "things you already deleted", How::Dir),
        t("pacman", "pacman package cache", vec![PathBuf::from("/var/cache/pacman/pkg")], Kind::Clean, pac_note, How::Term(pacman_clean)),
        t("yay", "AUR build cache (yay/paru)", vec![c.join("yay"), c.join("paru")], Kind::Clean, "AUR helpers re-download what they build", How::Dir),
        t("pip", "pip cache", vec![c.join("pip")], Kind::Clean, "downloaded wheels; pip re-downloads", How::Dir),
        t("npm", "npm cache", vec![h.join(".npm").join("_cacache")], Kind::Clean, "npm re-downloads what it needs", How::Dir),
        t("pnpm", "pnpm store (unused)", vec![data.join("pnpm").join("store")], Kind::Clean,
            "only packages no project uses are removed (pnpm store prune)", How::Pnpm),
        t("cargo", "cargo download cache", vec![h.join(".cargo").join("registry").join("cache")], Kind::Clean,
            "downloaded crates; cargo re-downloads", How::Dir),
        t("shaders", "shader caches", vec![c.join("mesa_shader_cache"), c.join("mesa_shader_cache_db"), c.join("nvidia").join("GLCache"), h.join(".nv").join("GLCache")],
            Kind::Clean, "games/GPU rebuild them (first launch may stutter)", How::Dir),
        t("thumbs", "thumbnail cache", vec![c.join("thumbnails")], Kind::Clean, "file managers redraw previews", How::Dir),
        t("browser", "browser caches", vec![c.join("mozilla"), c.join("google-chrome"), c.join("chromium"), c.join("BraveSoftware")],
            Kind::Clean, "close the browser first; logins and history are kept", How::Dir),
    ];
    v.insert(2, journal);
    v
}

/// C:\$Recycle.Bin\<your SID>.
fn recycle_bin_dir() -> Option<PathBuf> {
    let out = super::sys::run("whoami", &["/user", "/fo", "csv", "/nh"])?;
    let sid = out.trim().rsplit(',').next()?.trim_matches('"').to_string();
    sid.starts_with("S-1-").then(|| PathBuf::from(r"C:\$Recycle.Bin").join(sid))
}

/// Resolve the found-later rows' paths.
pub fn find_paths(tg: &Target, stop: &AtomicBool) -> Vec<PathBuf> {
    match &tg.find {
        Find::Fixed | Find::Journal => tg.paths.clone(),
        Find::NodeModules(root) => node_modules_dirs(root, stop),
        Find::ElectronDbs(root) => fs::read_dir(root)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.metadata().map(|m| m.is_dir() && !is_link(&m)).unwrap_or(false))
                    .map(|e| e.path().join("IndexedDB"))
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default(),
        Find::CacheOther(c) => fs::read_dir(c)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| !CLAIMED_CACHE.contains(&e.file_name().to_string_lossy().as_ref()))
                    .filter(|e| e.metadata().map(|m| !is_link(&m)).unwrap_or(false))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Every node_modules folder up to 4 levels under `root` (not inside hidden folders or _archive, never inside
/// another node_modules).
pub fn node_modules_dirs(root: &Path, stop: &AtomicBool) -> Vec<PathBuf> {
    fn walk(d: &Path, lvl: u32, out: &mut Vec<PathBuf>, stop: &AtomicBool) {
        if lvl > 4 || stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(rd) = fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if !md.is_dir() || is_link(&md) {
                continue;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == "node_modules" {
                out.push(e.path());
            } else if !name.starts_with('.') && name != "_archive" && name != "AppData" {
                walk(&e.path(), lvl + 1, out, stop);
            }
        }
    }
    let mut out = vec![];
    walk(root, 0, &mut out, stop);
    out
}

/// `journalctl --disk-usage`: "Archived and active journals take up 1.2G in the file system."
pub fn parse_journal(s: &str) -> Option<u64> {
    let i = s.find("take up ")?;
    let tok = s[i + 8..].split_whitespace().next()?;
    parse_size(tok)
}

/// "1.2G", "512.0M", "8K", "12.3 MiB" (unit may be a separate token: pass it joined) -> bytes.
pub fn parse_size(tok: &str) -> Option<u64> {
    let tok = tok.trim();
    let split = tok.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ',')).unwrap_or(tok.len());
    let n: f64 = tok[..split].replace(',', ".").parse().ok()?;
    let unit = tok[split..].trim().to_ascii_uppercase();
    let mul = match unit.chars().next() {
        None | Some('B') => 1.0,
        Some('K') => 1024.0,
        Some('M') => 1024.0 * 1024.0,
        Some('G') => 1024.0 * 1024.0 * 1024.0,
        Some('T') => 1024f64.powi(4),
        _ => return None,
    };
    Some((n * mul) as u64)
}

// ------------------------------------------------------------------ drives

#[derive(Clone, Debug)]
pub struct Drive {
    pub name: String,
    pub total: u64,
    pub free: u64,
}

pub fn drives() -> Vec<Drive> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut out: Vec<Drive> = vec![];
    let mut seen = std::collections::HashSet::new();
    for d in disks.list() {
        let fs_name = d.file_system().to_string_lossy().to_lowercase();
        if d.total_space() == 0 || matches!(fs_name.as_str(), "tmpfs" | "devtmpfs" | "overlay" | "squashfs" | "efivarfs" | "ramfs") {
            continue;
        }
        let mount = d.mount_point().to_string_lossy().to_string();
        if !cfg!(windows) && (mount.starts_with("/boot") || mount == "/efi" || mount.starts_with("/snap") || mount.starts_with("/var/lib/docker")) {
            continue;
        }
        // btrfs subvolumes (/, /home, /var/log...) share one device: show it once
        if !seen.insert(d.name().to_string_lossy().to_string()) {
            continue;
        }
        let name = if cfg!(windows) { mount.trim_end_matches('\\').to_string() } else { mount };
        out.push(Drive { name, total: d.total_space(), free: d.available_space() });
    }
    out
}
