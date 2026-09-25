//! Installed apps and package search: the Windows Uninstall registry keys (via `reg query`) and winget, or
//! pacman/yay (dpkg/rpm/apt as fallbacks) on Linux. Only reads; the install/uninstall commands it builds are run
//! by the pane in a terminal pane after the user confirms.

use super::scan::{Cmd, parse_size};
use crate::config::which;
use std::process::{Command, Stdio};

/// Run a program hidden (no console window flashes on Windows) and return its stdout, or None if it failed to
/// start. Never used for anything that changes the system.
pub fn run(prog: &str, args: &[&str]) -> Option<String> {
    let mut c = Command::new(prog);
    c.args(args).stdin(Stdio::null()).stderr(Stdio::null()).env("LANG", "C").env("LC_ALL", "C");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = c.output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Clone, Debug)]
pub struct App {
    pub name: String,
    pub version: String,
    /// publisher (Windows) or description (Linux)
    pub publisher: String,
    pub size: u64,
    pub date: String,
    pub uninstall: Cmd,
}

#[derive(Clone, Debug)]
pub struct Found {
    pub name: String,
    pub id: String,
    pub version: String,
    pub source: String,
    pub desc: String,
    pub install: Cmd,
}

// ------------------------------------------------------------------ installed apps

pub fn installed_apps() -> Result<Vec<App>, String> {
    if cfg!(windows) {
        let mut all = String::new();
        for key in [
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
            r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
            r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ] {
            all.push_str(&run("reg", &["query", key, "/s"]).unwrap_or_default());
            all.push('\n');
        }
        let apps = parse_reg(&all);
        if apps.is_empty() { Err("couldn't read the Uninstall registry keys".into()) } else { Ok(apps) }
    } else if which("pacman").is_some() {
        Ok(parse_pacman_qi(&run("pacman", &["-Qei"]).unwrap_or_default()))
    } else if which("dpkg-query").is_some() {
        let out = run("dpkg-query", &["-W", "-f=${Package}\t${Version}\t${Installed-Size}\t${binary:Summary}\n"]).unwrap_or_default();
        Ok(parse_tsv(&out, 1024, |n| Cmd::new("sudo", &["apt", "remove", n])))
    } else if which("rpm").is_some() {
        let out = run("rpm", &["-qa", "--qf", "%{NAME}\t%{VERSION}\t%{SIZE}\t%{SUMMARY}\n"]).unwrap_or_default();
        Ok(parse_tsv(&out, 1, |n| Cmd::new("sudo", &["dnf", "remove", n])))
    } else {
        Err("no pacman, dpkg or rpm found".into())
    }
}

/// `reg query <Uninstall key> /s` output -> apps with a name and an uninstaller, minus system components and
/// updates (ParentKeyName), deduped by name, sorted.
pub fn parse_reg(text: &str) -> Vec<App> {
    let mut apps: Vec<App> = vec![];
    let mut seen = std::collections::HashSet::new();
    let mut cur: Vec<(String, String)> = vec![];
    let mut flush = |cur: &mut Vec<(String, String)>| {
        let get = |k: &str| cur.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let name = get("DisplayName");
        let un = get("UninstallString");
        let un = if un.is_empty() { get("QuietUninstallString") } else { un };
        let system = get("SystemComponent") == "0x1";
        if !name.is_empty() && !un.is_empty() && !system && get("ParentKeyName").is_empty() && seen.insert(name.clone()) {
            let size = u64::from_str_radix(get("EstimatedSize").trim_start_matches("0x"), 16).unwrap_or(0) * 1024;
            let d = get("InstallDate");
            let date = if d.len() == 8 && d.chars().all(|c| c.is_ascii_digit()) { format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..]) } else { String::new() };
            apps.push(App { name, version: get("DisplayVersion"), publisher: get("Publisher"), size, date, uninstall: split_uninstall(&un) });
        }
        cur.clear();
    };
    for line in text.lines() {
        if line.starts_with("HKEY_") {
            flush(&mut cur);
        } else if let Some(rest) = line.strip_prefix("    ") {
            let mut parts = rest.splitn(3, "    ");
            if let (Some(n), Some(ty), v) = (parts.next(), parts.next(), parts.next()) {
                if ty.starts_with("REG_") {
                    cur.push((n.trim().to_string(), v.unwrap_or("").trim().to_string()));
                }
            }
        }
    }
    flush(&mut cur);
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

/// An UninstallString -> program + arguments (so the terminal pane doesn't need cmd.exe quoting). MsiExec /I
/// (repair/modify) becomes /X (uninstall).
pub fn split_uninstall(s: &str) -> Cmd {
    let s = s.trim();
    let (prog, rest) = if let Some(r) = s.strip_prefix('"') {
        match r.find('"') {
            Some(i) => (r[..i].to_string(), r[i + 1..].to_string()),
            None => (r.to_string(), String::new()),
        }
    } else if let Some(i) = s.to_ascii_lowercase().find(".exe") {
        (s[..i + 4].to_string(), s[i + 4..].to_string())
    } else {
        let mut it = s.splitn(2, ' ');
        (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string())
    };
    let msi = prog.to_ascii_lowercase().contains("msiexec");
    let args = rest
        .split_whitespace()
        .map(|a| {
            if msi && (a.starts_with("/I") || a.starts_with("/i")) { format!("/X{}", &a[2..]) } else { a.to_string() }
        })
        .collect();
    Cmd { prog, args }
}

/// `pacman -Qei` blocks -> apps with installed size and date.
pub fn parse_pacman_qi(text: &str) -> Vec<App> {
    let mut apps = vec![];
    for block in text.split("\n\n") {
        let get = |k: &str| {
            block.lines().find_map(|l| {
                let (key, v) = l.split_once(':')?;
                (key.trim() == k).then(|| v.trim().to_string())
            })
        };
        let Some(name) = get("Name") else { continue };
        let size = get("Installed Size").and_then(|s| parse_size(&s)).unwrap_or(0);
        let date = get("Install Date").unwrap_or_default();
        apps.push(App {
            uninstall: Cmd::new("sudo", &["pacman", "-Rns", &name]),
            name,
            version: get("Version").unwrap_or_default(),
            publisher: get("Description").unwrap_or_default(),
            size,
            date: short_date(&date),
        });
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

/// "Mon 01 Sep 2026 10:11:12 AM CEST" -> "01 Sep 2026" (whatever pacman prints, kept short).
fn short_date(d: &str) -> String {
    let w: Vec<&str> = d.split_whitespace().collect();
    if w.len() >= 4 { format!("{} {} {}", w[1], w[2], w[3]) } else { d.to_string() }
}

fn parse_tsv(text: &str, unit: u64, un: impl Fn(&str) -> Cmd) -> Vec<App> {
    let mut apps: Vec<App> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 3).then(|| App {
                name: f[0].into(),
                version: f[1].into(),
                size: f[2].trim().parse::<u64>().unwrap_or(0) * unit,
                publisher: f.get(3).unwrap_or(&"").to_string(),
                date: String::new(),
                uninstall: un(f[0]),
            })
        })
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

// ------------------------------------------------------------------ package search

pub fn search(q: &str) -> Result<Vec<Found>, String> {
    if cfg!(windows) {
        if which("winget").is_none() {
            return Err("winget isn't installed".into());
        }
        let out = run("winget", &["search", q, "--accept-source-agreements", "--disable-interactivity"]).unwrap_or_default();
        Ok(parse_winget(&out))
    } else if which("yay").is_some() || which("paru").is_some() {
        let helper = if which("yay").is_some() { "yay" } else { "paru" };
        Ok(parse_pacman_ss(&run(helper, &["-Ss", "--color", "never", q]).unwrap_or_default(), Some(helper)))
    } else if which("pacman").is_some() {
        Ok(parse_pacman_ss(&run("pacman", &["-Ss", "--color", "never", q]).unwrap_or_default(), None))
    } else if which("apt-cache").is_some() {
        let out = run("apt-cache", &["search", q]).unwrap_or_default();
        Ok(out
            .lines()
            .filter_map(|l| {
                let (n, d) = l.split_once(" - ")?;
                Some(Found { name: n.into(), id: n.into(), version: String::new(), source: "apt".into(), desc: d.into(),
                    install: Cmd::new("sudo", &["apt", "install", n]) })
            })
            .take(100)
            .collect())
    } else {
        Err("no winget, pacman or apt found".into())
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for c in it.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `winget search` table -> results. Columns are found from the header by character position; the progress
/// spinner before it (carriage returns) is dropped.
pub fn parse_winget(text: &str) -> Vec<Found> {
    let lines: Vec<Vec<char>> = strip_ansi(text).lines().map(|l| l.rsplit('\r').next().unwrap_or("").trim_end().chars().collect()).collect();
    let Some(head) = lines.iter().position(|l| {
        let s: String = l.iter().collect();
        s.trim_start().starts_with("Name") && s.contains(" Id ")
    }) else {
        return vec![];
    };
    let h: String = lines[head].iter().collect();
    let off = h.len() - h.trim_start().len();
    let col = |name: &str| h.find(&format!(" {name}")).map(|i| h[..i + 1].chars().count());
    let (Some(id), Some(ver)) = (col("Id"), col("Version")) else { return vec![] };
    let src = col("Source");
    let mat = col("Match");
    let cut = |l: &[char], a: usize, b: Option<usize>| -> String {
        let b = b.unwrap_or(l.len()).min(l.len());
        if a >= b { String::new() } else { l[a..b].iter().collect::<String>().trim().to_string() }
    };
    lines[head + 1..]
        .iter()
        .filter(|l| !l.is_empty() && !l.iter().all(|c| *c == '-' || *c == '─'))
        .filter(|l| l.len() > ver)
        .filter_map(|l| {
            let wid = cut(l, id, Some(ver));
            if wid.is_empty() || wid.contains(' ') {
                return None;
            }
            let version = cut(l, ver, mat.or(src)).split_whitespace().next().unwrap_or("").to_string();
            Some(Found {
                name: cut(l, off, Some(id)),
                install: Cmd::new("winget", &["install", "--id", &wid, "-e"]),
                id: wid,
                version,
                source: src.map(|s| cut(l, s, None)).unwrap_or_default(),
                desc: String::new(),
            })
        })
        .take(100)
        .collect()
}

/// `pacman -Ss` / `yay -Ss`: "repo/name version [installed]" then an indented description line.
pub fn parse_pacman_ss(text: &str, helper: Option<&str>) -> Vec<Found> {
    let text = strip_ansi(text);
    let mut out: Vec<Found> = vec![];
    for l in text.lines() {
        if l.starts_with(' ') || l.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                if last.desc.is_empty() {
                    last.desc = l.trim().to_string();
                }
            }
            continue;
        }
        let mut w = l.split_whitespace();
        let (Some(full), Some(version)) = (w.next(), w.next()) else { continue };
        let Some((repo, name)) = full.split_once('/') else { continue };
        let installed = l.contains("[installed");
        let install = match helper {
            Some(h) => Cmd::new(h, &["-S", name]),
            None => Cmd::new("sudo", &["pacman", "-S", name]),
        };
        out.push(Found {
            name: name.into(),
            id: full.into(),
            version: version.into(),
            source: if installed { format!("{repo} ✓") } else { repo.into() },
            desc: String::new(),
            install,
        });
    }
    out.truncate(150);
    out
}
