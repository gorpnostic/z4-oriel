//! A repo's own settings for the checkouts agents work in: `<repo>/.oriel/project.toml`, next to its templates
//! (`.oriel/templates/`). Commit it and every clone gets the same setup. Everything in it is optional:
//!
//!     setup = "pnpm install"                  # runs in every new worktree (and the merge gate's) before anything else
//!     copy = [".env", ".env.*"]               # files git doesn't track, copied over from your checkout first
//!     run = "pnpm dev --port $ORIEL_PORT"     # what T starts in a checkout
//!     ports = "5400-5499"                     # each checkout gets its own ORIEL_PORT from here
//!
//! A worktree is a fresh checkout: without `setup` a JS or Python project has no dependencies in it (the gate and
//! T borrow your node_modules instead, see git::link_deps), and without `copy` there's no .env.

use super::git;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Project {
    pub setup: String,
    pub copy: Vec<String>,
    pub run: String,
    pub ports: String,
}

/// Where ORIEL_PORT comes from when the file doesn't say.
pub const DEFAULT_PORTS: (u16, u16) = (5400, 5499);

pub fn path(repo: &Path) -> PathBuf {
    repo.join(".oriel").join("project.toml")
}

/// The repo's settings (all empty when it has no file). A file that doesn't parse is an error saying why.
pub fn load(repo: &Path) -> Result<Project, String> {
    let Ok(s) = std::fs::read_to_string(path(repo)) else { return Ok(Project::default()) };
    toml::from_str(&s).map_err(|e| format!(".oriel/project.toml: {}", e.to_string().lines().find(|l| !l.trim().is_empty()).unwrap_or("doesn't parse").trim()))
}

impl Project {
    /// Does it ask for anything a checkout should get?
    pub fn is_set(&self) -> bool {
        !self.setup.trim().is_empty() || !self.copy.is_empty() || !self.run.trim().is_empty() || !self.ports.trim().is_empty()
    }

    /// "5400-5499" (or one port) → the range; anything else is the default.
    pub fn port_range(&self) -> (u16, u16) {
        let s = self.ports.trim();
        let (a, b) = s.split_once('-').unwrap_or((s, s));
        match (a.trim().parse::<u16>(), b.trim().parse::<u16>()) {
            (Ok(a), Ok(b)) if a > 0 && a <= b => (a, b),
            _ => DEFAULT_PORTS,
        }
    }
}

/// Copy the `copy` files from `repo` into `checkout` (a `*` matches within one folder), leaving any the checkout
/// already has. Returns what was copied.
pub fn copy_files(p: &Project, repo: &Path, checkout: &Path) -> Vec<String> {
    let mut out = vec![];
    for entry in &p.copy {
        let entry = entry.trim().replace('\\', "/");
        // only files inside the repo
        if entry.is_empty() || entry.split('/').any(|s| s == "..") || Path::new(&entry).is_absolute() {
            continue;
        }
        let rels: Vec<String> = if entry.contains(['*', '?']) {
            let (dir, _) = entry.rsplit_once('/').unwrap_or(("", &entry));
            std::fs::read_dir(repo.join(dir))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| if dir.is_empty() { e.file_name().to_string_lossy().to_string() } else { format!("{dir}/{}", e.file_name().to_string_lossy()) })
                .filter(|rel| super::plan::glob_match(&entry, rel))
                .collect()
        } else {
            vec![entry.clone()]
        };
        for rel in rels {
            let (from, to) = (repo.join(&rel), checkout.join(&rel));
            if !from.is_file() || to.exists() {
                continue;
            }
            if let Some(d) = to.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if std::fs::copy(&from, &to).is_ok() {
                out.push(rel);
            }
        }
    }
    out
}

/// Get a fresh checkout ready: the `copy` files, then `setup` (with ORIEL_PORT when there is one) in the gate
/// shell. Without a setup, `borrow` gives it your node_modules through a link instead (the gate's checkout, T).
/// Err = what failed, with its first lines.
pub fn prepare(p: &Project, repo: &Path, checkout: &Path, port: u16, timeout: Duration, borrow: bool) -> Result<(), String> {
    copy_files(p, repo, checkout);
    if p.setup.trim().is_empty() {
        if borrow {
            git::link_deps(repo, checkout);
        }
        return Ok(());
    }
    // it installs its own: never through a link into your checkout's node_modules
    git::unlink_deps(checkout);
    let port = port.to_string();
    let env: Vec<(&str, &str)> = if port != "0" { vec![("ORIEL_PORT", port.as_str())] } else { vec![] };
    git::run_gate(checkout, p.setup.trim(), timeout, &env).map_err(|e| format!("setup {e}"))
}

/// A command with ORIEL_PORT filled in (`$ORIEL_PORT`, `${ORIEL_PORT}`, `$env:ORIEL_PORT`, `%ORIEL_PORT%`), so it
/// reads the same in any shell.
pub fn with_port(cmd: &str, port: u16) -> String {
    if port == 0 {
        return cmd.to_string();
    }
    let n = port.to_string();
    cmd.replace("${ORIEL_PORT}", &n).replace("$env:ORIEL_PORT", &n).replace("%ORIEL_PORT%", &n).replace("$ORIEL_PORT", &n)
}

/// A port for a new checkout: the lowest in `range` that no other checkout holds (`taken`) and nothing on this
/// machine listens on. 0 = the range is full.
pub fn free_port(range: (u16, u16), taken: &[u16]) -> u16 {
    (range.0..=range.1).find(|p| !taken.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_project_settings() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-scratch").join(format!("agents-project-{}", std::process::id()));
        let (repo, wt) = (dir.join("repo"), dir.join("wt"));
        std::fs::create_dir_all(repo.join(".oriel")).unwrap();
        std::fs::create_dir_all(repo.join("config")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        assert_eq!(load(&dir.join("nothing")), Ok(Project::default()), "no file, nothing set");
        std::fs::write(path(&repo), "setup = \"pnpm install\"\ncopy = [\".env\", \".env.*\", \"config/*.local.json\", \"../secret\", \"missing.txt\"]\nrun = \"pnpm dev --port $ORIEL_PORT\"\nports = \"6100-6102\"\n").unwrap();
        let p = load(&repo).unwrap();
        assert!(p.is_set() && p.setup == "pnpm install" && p.port_range() == (6100, 6102));
        assert_eq!(Project { ports: "junk".into(), ..Default::default() }.port_range(), DEFAULT_PORTS);
        assert_eq!(Project { ports: "7000".into(), ..Default::default() }.port_range(), (7000, 7000));
        // copy: plain names and globs, never out of the repo, never over what the checkout has
        for f in [".env", ".env.local", "config/app.local.json", "config/app.json"] {
            std::fs::write(repo.join(f), f).unwrap();
        }
        std::fs::write(wt.join(".env.local"), "the checkout's own").unwrap();
        let mut got = copy_files(&p, &repo, &wt);
        got.sort();
        assert_eq!(got, vec![".env", "config/app.local.json"]);
        assert_eq!(std::fs::read_to_string(wt.join(".env.local")).unwrap(), "the checkout's own");
        assert!(!wt.join("config/app.json").exists());
        // a broken file says so
        std::fs::write(path(&repo), "setup = [nope").unwrap();
        assert!(load(&repo).unwrap_err().starts_with(".oriel/project.toml: "));
        // ports: filled into the command whatever the shell's syntax; a free one outside what's taken
        assert_eq!(with_port("pnpm dev --port $ORIEL_PORT", 6101), "pnpm dev --port 6101");
        assert_eq!(with_port("serve -p %ORIEL_PORT% & echo ${ORIEL_PORT} $env:ORIEL_PORT", 7), "serve -p 7 & echo 7 7");
        assert_eq!(with_port("x $ORIEL_PORT", 0), "x $ORIEL_PORT");
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = held.local_addr().unwrap().port();
        assert_eq!(free_port((busy, busy), &[]), 0, "something listens there");
        let p = free_port((busy.saturating_sub(3).max(1025), busy), &[busy.saturating_sub(3).max(1025)]);
        assert!(p != 0 && p != busy && p != busy.saturating_sub(3).max(1025), "{p}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
