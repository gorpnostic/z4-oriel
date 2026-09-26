//! Self-update: find the newest release on GitHub, download the build for this machine, swap it in (keeping the
//! current binary so `oriel rollback` can put it back), and show the release notes.
//!
//! A running oriel.exe can't be overwritten on Windows but it can be renamed, so the old one steps aside as
//! `oriel.exe.old-<time>` and the new one takes its name; the change applies the next time oriel starts.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const REPO: &str = "gorpnostic/z4-oriel";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Release {
    /// "0.6.2"
    pub version: String,
    /// what changed, as written for the release (markdown)
    pub notes: String,
    /// the download for this machine
    pub asset: String,
    pub published: String,
}

/// "v0.6.10" -> (0, 6, 10); anything unparsable sorts first.
pub fn parse(v: &str) -> (u64, u64, u64) {
    let mut it = v.trim().trim_start_matches('v').split('.').map(|x| x.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("").parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

pub fn newer(candidate: &str, than: &str) -> bool {
    parse(candidate) > parse(than)
}

fn asset_name() -> &'static str {
    if cfg!(windows) { "oriel-windows-x86_64.zip" } else { "oriel-linux-x86_64.tar.gz" }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(60))).build().into()
}

fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let r = agent().get(url).header("User-Agent", "oriel").header("Accept", "application/vnd.github+json").call().map_err(|e| format!("couldn't reach GitHub: {e}"))?;
    let text = r.into_body().read_to_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

fn release_of(v: &serde_json::Value) -> Option<Release> {
    let tag = v["tag_name"].as_str()?;
    let asset = v["assets"].as_array()?.iter().find(|a| a["name"] == asset_name()).and_then(|a| a["browser_download_url"].as_str()).unwrap_or("").to_string();
    Some(Release { version: tag.trim_start_matches('v').to_string(), notes: v["body"].as_str().unwrap_or("").to_string(), asset, published: v["published_at"].as_str().unwrap_or("").to_string() })
}

/// The recent releases, newest first (drafts and pre-releases left out).
pub fn releases() -> Result<Vec<Release>, String> {
    let v = get_json(&format!("https://api.github.com/repos/{REPO}/releases?per_page=15"))?;
    Ok(v.as_array().into_iter().flatten().filter(|r| r["draft"] != true && r["prerelease"] != true).filter_map(release_of).collect())
}

/// Release notes for every version after `from` up to the newest, newest first, as one markdown text.
pub fn notes_since(all: &[Release], from: &str) -> String {
    let mut out = String::new();
    for r in all.iter().filter(|r| newer(&r.version, from)) {
        let body = clean_notes(&r.notes);
        out.push_str(&format!("## {}\n\n{}\n\n", r.version, if body.is_empty() { "(no notes)".into() } else { body }));
    }
    out.trim_end().to_string()
}

/// GitHub's own "Full Changelog: …compare…" line says nothing to a reader here.
fn clean_notes(s: &str) -> String {
    s.lines().filter(|l| !l.contains("**Full Changelog**")).collect::<Vec<_>>().join("\n").trim().to_string()
}

// ------------------------------------------------------------------ "is there a newer one?" once a day
#[derive(Serialize, Deserialize, Default)]
struct Checked {
    at: i64,
    releases: Vec<Release>,
}

fn state_dir() -> PathBuf {
    if cfg!(test) {
        return std::path::absolute("target/test-scratch/update").unwrap_or_default();
    }
    crate::config::data_dir().join("update")
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The releases, from a check at most a day old (or a fresh one; `force` always asks GitHub).
pub fn check(force: bool) -> Result<Vec<Release>, String> {
    let file = state_dir().join("checked.json");
    if !force {
        if let Some(c) = std::fs::read_to_string(&file).ok().and_then(|s| serde_json::from_str::<Checked>(&s).ok()) {
            if now() - c.at < 24 * 3600 && !c.releases.is_empty() {
                return Ok(c.releases);
            }
        }
    }
    let rs = releases()?;
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::write(&file, serde_json::to_string(&Checked { at: now(), releases: rs.clone() }).unwrap_or_default());
    Ok(rs)
}

/// The newest release if it's newer than this one.
pub fn available(rs: &[Release]) -> Option<Release> {
    rs.iter().filter(|r| newer(&r.version, VERSION)).max_by_key(|r| parse(&r.version)).cloned()
}

// ------------------------------------------------------------------ installing and rolling back
fn backups_dir() -> PathBuf {
    state_dir().join("previous")
}

fn exe_name() -> &'static str {
    if cfg!(windows) { "oriel.exe" } else { "oriel" }
}

/// Put `new` where the running binary is. The old one moves aside first (Windows can rename a running exe but
/// not overwrite it); leftovers from earlier updates are cleared when they're no longer running.
fn swap_in(new: &Path, exe: &Path) -> Result<PathBuf, String> {
    let exe = exe.to_path_buf();
    let dir = exe.parent().ok_or("no folder")?.to_path_buf();
    for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        if e.file_name().to_string_lossy().starts_with(&format!("{}.old", exe_name())) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    if cfg!(windows) {
        let aside = dir.join(format!("{}.old-{}", exe_name(), now()));
        std::fs::rename(&exe, &aside).map_err(|e| format!("couldn't move the current oriel aside: {e}"))?;
        if let Err(e) = std::fs::copy(new, &exe) {
            let _ = std::fs::rename(&aside, &exe); // put it back
            return Err(format!("couldn't put the new oriel in place: {e}"));
        }
    } else {
        let tmp = dir.join(format!(".{}.new", exe_name()));
        std::fs::copy(new, &tmp).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::rename(&tmp, &exe).map_err(|e| format!("couldn't put the new oriel in place: {e}"))?;
    }
    Ok(exe)
}

/// Keep a copy of the binary that's running now, for `oriel rollback` (the last three versions).
fn back_up_current(exe: &Path, version: &str) -> Result<(), String> {
    let dir = backups_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::copy(exe, dir.join(format!("oriel-{version}{}", if cfg!(windows) { ".exe" } else { "" }))).map_err(|e| format!("couldn't keep a copy of this version: {e}"))?;
    let mut all = backups();
    all.sort_by_key(|(v, _)| std::cmp::Reverse(parse(v)));
    for (_, p) in all.iter().skip(3) {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

/// The versions `oriel rollback` can go back to: (version, file).
pub fn backups() -> Vec<(String, PathBuf)> {
    std::fs::read_dir(backups_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            let v = n.strip_prefix("oriel-")?.trim_end_matches(".exe").to_string();
            Some((v, e.path()))
        })
        .collect()
}

fn extract(archive: &Path, into: &Path) -> Result<PathBuf, String> {
    // Windows' own bsdtar reads zips (and PATH may hold Git's GNU tar, which doesn't take drive letters)
    let tar = if cfg!(windows) { PathBuf::from(std::env::var("SystemRoot").unwrap_or("C:\\Windows".into())).join("System32").join("tar.exe") } else { PathBuf::from("tar") };
    let mut c = std::process::Command::new(tar);
    c.arg("-xf").arg(archive).arg("-C").arg(into);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    let out = c.output().map_err(|e| format!("couldn't unpack it: {e}"))?;
    if !out.status.success() {
        return Err(format!("couldn't unpack it: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let bin = into.join(exe_name());
    if !bin.is_file() {
        return Err("the download had no oriel in it".into());
    }
    Ok(bin)
}

/// Does this binary run and say the version we expect?
fn sane(bin: &Path, want: &str) -> Result<(), String> {
    let mut c = std::process::Command::new(bin);
    c.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    let out = c.output().map_err(|e| format!("the new oriel won't start: {e}"))?;
    let v = String::from_utf8_lossy(&out.stdout);
    if !v.contains(want) {
        return Err(format!("the download says it's '{}', not {want}", v.trim()));
    }
    Ok(())
}

/// Download `r`, check it runs, keep the current binary for rollback, and swap. `say` reports progress.
pub fn install(r: &Release, say: &dyn Fn(&str)) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    install_to(r, &exe, VERSION, say)
}

/// `install` for any binary: `exe` (running `version`) gets replaced.
pub fn install_to(r: &Release, exe: &Path, version: &str, say: &dyn Fn(&str)) -> Result<(), String> {
    if r.asset.is_empty() {
        return Err(format!("{} has no build for this machine", r.version));
    }
    let tmp = std::env::temp_dir().join(format!("oriel-update-{}", now()));
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let result = (|| {
        say(&format!("downloading {}", r.version));
        let archive = tmp.join(asset_name());
        let resp = agent().get(&r.asset).header("User-Agent", "oriel").call().map_err(|e| format!("download failed: {e}"))?;
        let mut f = std::fs::File::create(&archive).map_err(|e| e.to_string())?;
        std::io::copy(&mut resp.into_body().into_reader(), &mut f).map_err(|e| format!("download failed: {e}"))?;
        drop(f);
        say("unpacking");
        let bin = extract(&archive, &tmp)?;
        sane(&bin, &r.version)?;
        say("keeping this version for rollback");
        back_up_current(exe, version)?;
        say("swapping it in");
        swap_in(&bin, exe)?;
        let _ = std::fs::write(state_dir().join("updated.json"), serde_json::json!({"from": version, "to": r.version, "at": now()}).to_string());
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// Put the newest kept older version back. Returns its version.
pub fn rollback() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    rollback_to(&exe, VERSION)
}

/// `rollback` for any binary `exe` running `version`.
pub fn rollback_to(exe: &Path, version: &str) -> Result<String, String> {
    let mut all: Vec<(String, PathBuf)> = backups().into_iter().filter(|(v, _)| v != version).collect();
    all.sort_by_key(|(v, _)| std::cmp::Reverse(parse(v)));
    let Some((v, file)) = all.into_iter().next() else { return Err("no earlier version kept yet (one is kept each time oriel updates itself)".into()) };
    sane(&file, "oriel")?; // it starts and is oriel
    // copy it out first: swapping must not consume the kept file
    let staged = std::env::temp_dir().join(format!("oriel-rollback-{}{}", now(), if cfg!(windows) { ".exe" } else { "" }));
    std::fs::copy(&file, &staged).map_err(|e| e.to_string())?;
    back_up_current(exe, version)?;
    let r = swap_in(&staged, exe);
    let _ = std::fs::remove_file(&staged);
    r?;
    let _ = std::fs::write(state_dir().join("updated.json"), serde_json::json!({"from": version, "to": v, "at": now(), "rollback": true}).to_string());
    Ok(v)
}

/// Just updated (this is the first start of a new version)? Returns (from, to) once.
pub fn just_updated() -> Option<(String, String)> {
    let file = state_dir().join("updated.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).ok()?).ok()?;
    let (from, to) = (v["from"].as_str()?.to_string(), v["to"].as_str()?.to_string());
    if to != VERSION || v["seen"] == true {
        return None;
    }
    let mut v = v;
    v["seen"] = serde_json::Value::Bool(true);
    let _ = std::fs::write(&file, v.to_string());
    Some((from, to))
}

// ------------------------------------------------------------------ `oriel update` / `oriel rollback`
pub fn cli_update() -> i32 {
    println!("oriel {VERSION}: checking for updates…");
    let rs = match check(true) {
        Ok(rs) => rs,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let Some(r) = available(&rs) else {
        println!("you have the newest version.");
        return 0;
    };
    let notes = notes_since(&rs, VERSION);
    if !notes.is_empty() {
        println!("\nwhat's new:\n\n{notes}\n");
    }
    match install(&r, &|s| println!(":: {s}")) {
        Ok(()) => {
            println!(":: updated to {} — start oriel again to use it. If something's wrong: oriel rollback", r.version);
            0
        }
        Err(e) => {
            eprintln!("update failed: {e}");
            1
        }
    }
}

pub fn cli_rollback() -> i32 {
    match rollback() {
        Ok(v) => {
            println!(":: back to oriel {v} — start oriel again to use it");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_versions_and_notes() {
        assert!(newer("0.6.10", "0.6.9") && newer("v1.0.0", "0.9.9") && !newer("0.6.1", "0.6.1") && !newer("0.5.9", "0.6.0"));
        let rs = vec![
            Release { version: "0.7.0".into(), notes: "- calendar\n\n**Full Changelog**: https://x".into(), ..Default::default() },
            Release { version: "0.6.2".into(), notes: "- themes".into(), ..Default::default() },
            Release { version: "0.6.1".into(), notes: "- old".into(), ..Default::default() },
        ];
        let n = notes_since(&rs, "0.6.1");
        assert!(n.contains("## 0.7.0") && n.contains("- calendar") && n.contains("## 0.6.2") && !n.contains("- old") && !n.contains("Full Changelog"), "{n}");
        assert_eq!(parse("0.6.1-beta"), (0, 6, 1));
    }

    /// A real update and rollback, on a scratch copy (downloads the latest release):
    /// `cargo build; cargo test update_live_install -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn update_live_install() {
        let dir = std::path::absolute("target/test-scratch/update-install").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(backups_dir());
        std::fs::create_dir_all(&dir).unwrap();
        // "the old version": the dev build, pretending to be 0.0.1
        let exe = dir.join(exe_name());
        let dev = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join(exe_name());
        std::fs::copy(&dev, &exe).unwrap();
        let rs = releases().unwrap();
        let latest = available(&rs).unwrap_or_else(|| rs[0].clone());
        install_to(&latest, &exe, "0.0.1", &|s| println!(":: {s}")).unwrap();
        let v = std::process::Command::new(&exe).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&v.stdout).contains(&latest.version), "swapped in");
        assert!(backups().iter().any(|(v, _)| v == "0.0.1"), "the old one is kept");
        // and back
        let back = rollback_to(&exe, &latest.version).unwrap();
        assert_eq!(back, "0.0.1");
        let v = std::process::Command::new(&exe).arg("--version").output().unwrap();
        println!("after rollback: {}", String::from_utf8_lossy(&v.stdout).trim());
        assert!(!String::from_utf8_lossy(&v.stdout).contains(&format!("oriel {}
", latest.version)) || latest.version == VERSION);
    }

    /// The real thing against GitHub, read-only: `cargo test update_live_check -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn update_live_check() {
        let rs = releases().unwrap();
        println!("{} releases, newest {} ({})", rs.len(), rs[0].version, rs[0].asset);
        assert!(!rs.is_empty() && !rs[0].asset.is_empty());
    }
}
