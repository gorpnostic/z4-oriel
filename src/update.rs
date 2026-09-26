//! Self-update: find the newest release on GitHub, download the build for this machine, swap it in (keeping the
//! current binary so `oriel rollback` can put it back), and show the release notes.
//!
//! A running oriel.exe can't be overwritten on Windows but it can be renamed, so the old one steps aside as
//! `oriel.exe.old-<time>` and the new one takes its name; the change applies the next time oriel starts.
//!
//! After a swap, the file at the running binary's path is already another version (Windows keeps reporting the
//! old path, Linux calls it "… (deleted)"). So a copy is only kept for rollback when that file still says it's
//! the running version, and a rollback only goes to an older version than the running one.

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

/// Why an update or a check didn't happen. `oriel update` decides from this whether the install script could do
/// better (it uses plain download links and the system's own unzip) or would only make things worse.
#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    /// GitHub couldn't be reached at all (offline, DNS, a timeout): the install script can't reach it either
    Offline(String),
    /// GitHub answered with an error (rate limit, outage)
    Refused(String),
    /// the download wouldn't unpack here
    Unpack(String),
    /// the release has no build for this machine, or the new binary doesn't run or says the wrong version
    Bad(String),
    /// couldn't keep this version for rollback
    Backup(String),
    /// couldn't put the new binary in place
    Swap(String, std::io::ErrorKind),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Fail::Offline(s) | Fail::Refused(s) | Fail::Unpack(s) | Fail::Bad(s) | Fail::Backup(s) | Fail::Swap(s, _) => f.write_str(s),
        }
    }
}

impl Fail {
    /// Would the install script do better? Only when GitHub's API refused or unpacking failed; never after a bad
    /// download, a failed backup or when GitHub is unreachable.
    pub fn script_may_help(&self) -> bool {
        matches!(self, Fail::Refused(_) | Fail::Unpack(_))
    }
    /// The folder the binary lives in isn't writable by this user.
    pub fn denied(&self) -> bool {
        matches!(self, Fail::Swap(_, k) if matches!(k, std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem))
    }
}

fn net_fail(what: &str, e: ureq::Error) -> Fail {
    match e {
        ureq::Error::StatusCode(c) => Fail::Refused(format!("{what}: GitHub said {c}")),
        e => Fail::Offline(format!("{what}: {e}")),
    }
}

fn get_json(url: &str) -> Result<serde_json::Value, Fail> {
    let r = agent().get(url).header("User-Agent", "oriel").header("Accept", "application/vnd.github+json").call().map_err(|e| net_fail("couldn't reach GitHub", e))?;
    let text = r.into_body().read_to_string().map_err(|e| Fail::Offline(format!("couldn't read GitHub's answer: {e}")))?;
    serde_json::from_str(&text).map_err(|e| Fail::Refused(format!("GitHub's answer didn't make sense: {e}")))
}

fn release_of(v: &serde_json::Value) -> Option<Release> {
    let tag = v["tag_name"].as_str()?;
    let asset = v["assets"].as_array()?.iter().find(|a| a["name"] == asset_name()).and_then(|a| a["browser_download_url"].as_str()).unwrap_or("").to_string();
    Some(Release { version: tag.trim_start_matches('v').to_string(), notes: v["body"].as_str().unwrap_or("").to_string(), asset, published: v["published_at"].as_str().unwrap_or("").to_string() })
}

/// The recent releases, newest first (drafts and pre-releases left out).
pub fn releases() -> Result<Vec<Release>, Fail> {
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

#[cfg(test)]
thread_local! {
    /// Tests that install or roll back keep their state in a folder of their own.
    static TEST_STATE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn state_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(d) = TEST_STATE.with(|d| d.borrow().clone()) {
        return d;
    }
    if cfg!(test) {
        return std::path::absolute("target/test-scratch/update").unwrap_or_default();
    }
    crate::config::data_dir().join("update")
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A temp name no other update (in this process or another one, this second or the next) will pick.
fn unique(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

/// The running binary's path. Linux reports "…/oriel (deleted)" once the file has been replaced (by an update,
/// or the install script): that path doesn't exist, so it means the file now at the plain path. None of this
/// is guessed: the path has to exist, or updating, rolling back and pointing Claude's status line at it refuse.
pub fn exe_path() -> Result<PathBuf, String> {
    let p = std::env::current_exe().map_err(|e| format!("couldn't tell where this oriel is: {e}"))?;
    live_path(&p).ok_or_else(|| format!("{} is gone (moved or replaced since oriel started): restart oriel first", p.display()))
}

fn live_path(p: &Path) -> Option<PathBuf> {
    if p.exists() {
        return Some(p.to_path_buf());
    }
    let t = PathBuf::from(p.to_str()?.strip_suffix(" (deleted)")?);
    t.exists().then_some(t)
}

/// The releases, from a check at most a day old (or a fresh one; `force` always asks GitHub).
pub fn check(force: bool) -> Result<Vec<Release>, Fail> {
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

fn ext() -> &'static str {
    if cfg!(windows) { ".exe" } else { "" }
}

fn swap_fail(what: &str, e: std::io::Error) -> Fail {
    Fail::Swap(format!("{what}: {e}"), e.kind())
}

/// Put `new` where the running binary is. The old one moves aside first (Windows can rename a running exe but
/// not overwrite it); leftovers from earlier updates are cleared when they're no longer running.
fn swap_in(new: &Path, exe: &Path) -> Result<PathBuf, Fail> {
    let exe = exe.to_path_buf();
    let dir = exe.parent().ok_or(Fail::Swap("no folder".into(), std::io::ErrorKind::NotFound))?.to_path_buf();
    for e in std::fs::read_dir(&dir).map_err(|e| swap_fail("couldn't look in oriel's folder", e))?.flatten() {
        if e.file_name().to_string_lossy().starts_with(&format!("{}.old", exe_name())) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    if cfg!(windows) {
        // its own name: an older copy may still be running (and locked) from another window, maybe from this second
        let aside = dir.join(format!("{}.old-{}-{}", exe_name(), now(), std::process::id()));
        std::fs::rename(&exe, &aside).map_err(|e| swap_fail("couldn't move the current oriel aside", e))?;
        if let Err(e) = std::fs::copy(new, &exe) {
            let _ = std::fs::rename(&aside, &exe); // put it back
            return Err(swap_fail("couldn't put the new oriel in place", e));
        }
    } else {
        let tmp = dir.join(format!(".{}.new", exe_name()));
        std::fs::copy(new, &tmp).map_err(|e| swap_fail("couldn't write next to oriel", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::rename(&tmp, &exe).map_err(|e| swap_fail("couldn't put the new oriel in place", e))?;
    }
    Ok(exe)
}

/// Keep a copy of the binary that's running now, for `oriel rollback`: the three saved most recently, whatever
/// their version numbers (after a rollback the copy just saved has the highest number, and must not be the one
/// pruned).
fn back_up_current(exe: &Path, version: &str) -> Result<(), String> {
    let dir = backups_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let kept = dir.join(format!("oriel-{version}{}", ext()));
    std::fs::copy(exe, &kept).map_err(|e| format!("couldn't keep a copy of this version: {e}"))?;
    // a copy keeps the original's modified time on Windows: stamp it, so its age is when it was kept
    let _ = std::fs::File::options().write(true).open(&kept).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    let mut all: Vec<(std::time::SystemTime, PathBuf)> =
        backups().into_iter().map(|(_, p)| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), p)).collect();
    all.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, p) in all.iter().skip(3).filter(|(_, p)| *p != kept) {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

/// The versions kept for rollback: (version, file).
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

/// The kept versions older than `version`, newest first: what a rollback can go back to. (The version you
/// rolled back from is kept too, so "back" must never pick it.)
pub fn older_kept(version: &str) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = backups().into_iter().filter(|(v, _)| newer(version, v)).collect();
    v.sort_by_key(|(v, _)| std::cmp::Reverse(parse(v)));
    v
}

fn extract(archive: &Path, into: &Path) -> Result<PathBuf, Fail> {
    // Windows' own bsdtar reads zips (and PATH may hold Git's GNU tar, which doesn't take drive letters)
    let tar = if cfg!(windows) { PathBuf::from(std::env::var("SystemRoot").unwrap_or("C:\\Windows".into())).join("System32").join("tar.exe") } else { PathBuf::from("tar") };
    let mut c = std::process::Command::new(tar);
    c.arg("-xf").arg(archive).arg("-C").arg(into);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    let out = c.output().map_err(|e| Fail::Unpack(format!("couldn't unpack it: {e}")))?;
    if !out.status.success() {
        return Err(Fail::Unpack(format!("couldn't unpack it: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let bin = into.join(exe_name());
    if !bin.is_file() {
        return Err(Fail::Bad("the download had no oriel in it".into()));
    }
    Ok(bin)
}

/// What a binary says it is ("oriel 0.6.2"), from running it with --version.
fn version_of(bin: &Path) -> Result<String, String> {
    // tests stand a small text file in for a binary: "fake oriel 0.6.1"
    #[cfg(test)]
    if let Some(v) = std::fs::read_to_string(bin).ok().and_then(|s| s.strip_prefix("fake ").map(|v| v.trim().to_string())) {
        return Ok(v);
    }
    let mut c = std::process::Command::new(bin);
    c.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    let out = c.output().map_err(|e| format!("{} won't start: {e}", bin.display()))?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string())
}

/// Does this binary run and say exactly the version we expect?
fn sane(bin: &Path, want: &str) -> Result<(), String> {
    let v = version_of(bin)?;
    if v != format!("oriel {want}") {
        return Err(format!("it says it's '{v}', not oriel {want}"));
    }
    Ok(())
}

/// Keep the binary at `exe` for rollback, as long as it really is `version`. After an update in this same
/// process (or from another oriel window) the file there is already the new one, and saving it under the old
/// version's name would lose the real old copy.
fn keep_current(exe: &Path, version: &str, say: &dyn Fn(&str)) -> Result<(), Fail> {
    if let Err(e) = sane(exe, version) {
        say(&format!("not keeping a rollback copy: the oriel on disk isn't {version} any more ({e})"));
        return Ok(());
    }
    say("keeping this version for rollback");
    back_up_current(exe, version).map_err(Fail::Backup)
}

/// Remember the swap, for the "updated" / "rolled back" note on the next start.
pub fn note_updated(from: &str, to: &str, rollback: bool) {
    let _ = std::fs::create_dir_all(state_dir());
    let mut v = serde_json::json!({"from": from, "to": to, "at": now()});
    if rollback {
        v["rollback"] = serde_json::Value::Bool(true);
    }
    let _ = std::fs::write(state_dir().join("updated.json"), v.to_string());
}

fn download(url: &str, to: &Path) -> Result<(), Fail> {
    let resp = agent().get(url).header("User-Agent", "oriel").call().map_err(|e| net_fail("download failed", e))?;
    let mut f = std::fs::File::create(to).map_err(|e| Fail::Bad(format!("couldn't save the download: {e}")))?;
    std::io::copy(&mut resp.into_body().into_reader(), &mut f).map_err(|e| Fail::Offline(format!("download failed: {e}")))?;
    Ok(())
}

/// Download `r`, check it runs, keep the current binary for rollback, and swap. `say` reports progress.
pub fn install(r: &Release, say: &dyn Fn(&str)) -> Result<(), Fail> {
    let exe = exe_path().map_err(Fail::Bad)?;
    install_to(r, &exe, VERSION, say)
}

/// `install` for any binary: `exe` (running `version`) gets replaced.
pub fn install_to(r: &Release, exe: &Path, version: &str, say: &dyn Fn(&str)) -> Result<(), Fail> {
    if r.asset.is_empty() {
        return Err(Fail::Bad(format!("{} has no build for this machine", r.version)));
    }
    let tmp = unique("oriel-update");
    std::fs::create_dir_all(&tmp).map_err(|e| Fail::Bad(e.to_string()))?;
    let result = (|| {
        say(&format!("downloading {}", r.version));
        let archive = tmp.join(asset_name());
        download(&r.asset, &archive)?;
        say("unpacking");
        let bin = extract(&archive, &tmp)?;
        sane(&bin, &r.version).map_err(|e| Fail::Bad(format!("the download doesn't check out: {e}")))?;
        keep_current(exe, version, say)?;
        say("swapping it in");
        swap_in(&bin, exe)?;
        note_updated(version, &r.version, false);
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// Put the newest kept older version back. Returns its version.
pub fn rollback() -> Result<String, String> {
    rollback_to(&exe_path()?, VERSION)
}

/// `rollback` for any binary `exe` running `version`.
pub fn rollback_to(exe: &Path, version: &str) -> Result<String, String> {
    let Some((v, file)) = older_kept(version).into_iter().next() else { return Err("no older version kept yet (one is kept each time oriel updates itself)".into()) };
    // the file on disk must still be this version, or keeping it (below) would file it under the wrong name
    sane(exe, version).map_err(|e| format!("the oriel on disk has changed since this one started ({e}): restart oriel first"))?;
    sane(&file, &v).map_err(|e| format!("the kept copy of {v} is damaged ({e}); nothing changed"))?;
    // copy it out first: swapping must not consume the kept file
    let staged = PathBuf::from(format!("{}{}", unique("oriel-rollback").display(), ext()));
    std::fs::copy(&file, &staged).map_err(|e| e.to_string())?;
    let r = back_up_current(exe, version).and_then(|_| swap_in(&staged, exe).map_err(|e| e.to_string()));
    let _ = std::fs::remove_file(&staged);
    r?;
    note_updated(version, &v, true);
    Ok(v)
}

/// Just updated or rolled back (this is the first start of the new version)? Returns (from, to, rolled back),
/// once.
pub fn just_updated() -> Option<(String, String, bool)> {
    let file = state_dir().join("updated.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).ok()?).ok()?;
    let (from, to) = (v["from"].as_str()?.to_string(), v["to"].as_str()?.to_string());
    if to != VERSION || v["seen"] == true {
        return None;
    }
    let back = v["rollback"] == true;
    let mut v = v;
    v["seen"] = serde_json::Value::Bool(true);
    let _ = std::fs::write(&file, v.to_string());
    Some((from, to, back))
}

// ------------------------------------------------------------------ `oriel update` / `oriel rollback`
/// `oriel update`'s exit code when the in-place update failed in a way the install script may get past (GitHub's
/// API refused, or the download wouldn't unpack here): main.rs then runs the script, into the same folder.
pub const TRY_SCRIPT: i32 = 4;

fn exit_code(e: &Fail) -> i32 {
    match e {
        _ if e.script_may_help() => TRY_SCRIPT,
        _ if e.denied() => 3,
        Fail::Offline(_) => 2,
        _ => 1,
    }
}

/// Say so when the `oriel` a new terminal would run isn't the file that was just updated.
fn warn_if_shadowed(updated: &Path) {
    let same = |a: &Path, b: &Path| std::fs::canonicalize(a).ok().is_some_and(|a| std::fs::canonicalize(b).ok() == Some(a));
    if let Some(first) = crate::config::which("oriel").filter(|p| !same(p, updated)) {
        println!(":: note: `oriel` on your PATH is {}, not this one: remove that one, or put {} first", first.display(), updated.parent().unwrap_or(updated).display());
    }
}

pub fn cli_update() -> i32 {
    let exe = match exe_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!("oriel {VERSION} ({}): checking for updates…", exe.display());
    let rs = match check(true) {
        Ok(rs) => rs,
        Err(e) => {
            eprintln!("{e}");
            return exit_code(&e);
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
    match install_to(&r, &exe, VERSION, &|s| println!(":: {s}")) {
        Ok(()) => {
            println!(":: updated {} to {} — start oriel again to use it. If something's wrong: oriel rollback", exe.display(), r.version);
            warn_if_shadowed(&exe);
            0
        }
        Err(e) => {
            eprintln!("update failed: {e}");
            if e.denied() {
                let dir = exe.parent().unwrap_or(&exe).display().to_string();
                if cfg!(windows) {
                    eprintln!("{dir} isn't writable for you: run  oriel update  from a terminal opened as administrator");
                } else {
                    eprintln!("{dir} isn't writable for you: run it as the folder's owner (sudo oriel update), or update oriel the way you installed it");
                }
            }
            exit_code(&e)
        }
    }
}

/// The install script, the last resort after TRY_SCRIPT: into the folder this oriel runs from (not the script's
/// default folder, which may not be the `oriel` you run), then remember the update and say which file changed.
pub fn cli_script_fallback() -> i32 {
    let exe = match exe_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let Some(dir) = exe.parent() else { return 1 };
    println!(":: trying the install script instead, into {}", dir.display());
    let raw = format!("https://raw.githubusercontent.com/{REPO}/master");
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("powershell.exe");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &format!("irm {raw}/install.ps1 | iex")]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", &format!("curl -fsSL {raw}/install.sh | sh")]);
        c
    };
    match c.env("ORIEL_BIN_DIR", dir).status() {
        Ok(s) if s.success() => {}
        Ok(s) => return s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("couldn't run the install script: {e}");
            return 1;
        }
    }
    let bin = dir.join(exe_name());
    let to = version_of(&bin).ok().and_then(|v| v.strip_prefix("oriel ").map(String::from)).unwrap_or_default();
    if !to.is_empty() && to != VERSION {
        note_updated(VERSION, &to, false);
    }
    println!(":: updated {}{}", bin.display(), if to.is_empty() { String::new() } else { format!(" to {to}") });
    warn_if_shadowed(&bin);
    0
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

    /// A stand-in binary: under test, version_of reads "fake oriel X" out of it instead of running it.
    fn fake(path: &Path, v: &str) {
        std::fs::write(path, format!("fake oriel {v}")).unwrap();
    }

    /// A scratch folder with bin/ for the "installed" oriel, and this test's own update state.
    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let d = std::path::absolute(format!("target/test-scratch/update-{name}")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("bin")).unwrap();
        TEST_STATE.with(|s| *s.borrow_mut() = Some(d.join("state")));
        let exe = d.join("bin").join(exe_name());
        (d, exe)
    }

    fn pause() {
        std::thread::sleep(std::time::Duration::from_millis(20)); // distinct save times
    }

    #[test]
    fn update_rollback_goes_back_never_forward() {
        let (_d, exe) = scratch("rollback");
        // 0.6.0 -> 0.6.1 -> 0.6.2 kept 0.6.0 and 0.6.1
        for v in ["0.6.0", "0.6.1"] {
            fake(&exe, v);
            back_up_current(&exe, v).unwrap();
            pause();
        }
        fake(&exe, "0.6.2");
        assert_eq!(rollback_to(&exe, "0.6.2").unwrap(), "0.6.1");
        assert_eq!(version_of(&exe).unwrap(), "oriel 0.6.1");
        assert!(backups().iter().any(|(v, _)| v == "0.6.2"), "the version left is kept too");
        // a second rollback goes further back, not forward to the 0.6.2 just left
        pause();
        assert_eq!(rollback_to(&exe, "0.6.1").unwrap(), "0.6.0");
        assert_eq!(version_of(&exe).unwrap(), "oriel 0.6.0");
        assert_eq!(older_kept("0.6.0").len(), 0);
        assert!(rollback_to(&exe, "0.6.0").unwrap_err().contains("no older version"));
        // the next start says "rolled back", once
        note_updated("9.9.9", VERSION, true);
        assert_eq!(just_updated(), Some(("9.9.9".into(), VERSION.into(), true)));
        assert_eq!(just_updated(), None);
        note_updated("0.0.1", VERSION, false);
        assert_eq!(just_updated(), Some(("0.0.1".into(), VERSION.into(), false)));
    }

    #[test]
    fn update_second_swap_keeps_the_real_old_copy() {
        let (_d, exe) = scratch("twice");
        fake(&exe, "0.6.1");
        back_up_current(&exe, "0.6.1").unwrap();
        pause();
        // this process runs 0.6.2 and updates to 0.7.0: 0.6.2 is kept
        let said = std::sync::Mutex::new(vec![]);
        let say = |s: &str| said.lock().unwrap().push(s.to_string());
        fake(&exe, "0.6.2");
        keep_current(&exe, "0.6.2", &say).unwrap();
        fake(&exe, "0.7.0"); // what the swap put there
        // a second update in the same process (or from a second window) must not file 0.7.0 as 0.6.2
        keep_current(&exe, "0.6.2", &say).unwrap();
        let kept = backups().into_iter().find(|(v, _)| v == "0.6.2").unwrap().1;
        assert_eq!(version_of(&kept).unwrap(), "oriel 0.6.2", "the real 0.6.2 is still kept");
        assert!(said.lock().unwrap().last().unwrap().contains("not keeping a rollback copy"), "{:?}", said.lock().unwrap());
        // and a rollback from here refuses rather than swapping blind
        assert!(rollback_to(&exe, "0.6.2").unwrap_err().contains("restart oriel first"));
        assert_eq!(version_of(&exe).unwrap(), "oriel 0.7.0", "nothing changed");
        // a kept copy that isn't the version its name says is refused too
        fake(&kept, "0.7.0");
        assert!(rollback_to(&exe, "0.7.0").unwrap_err().contains("damaged"));
    }

    #[test]
    fn update_keeps_the_three_saved_last() {
        let (_d, exe) = scratch("prune");
        for v in ["0.6.3", "0.6.0", "0.6.1", "0.6.2"] {
            fake(&exe, v);
            back_up_current(&exe, v).unwrap();
            pause();
        }
        // by version number 0.6.0 would go; by age it's 0.6.3, kept first
        let mut kept: Vec<String> = backups().into_iter().map(|(v, _)| v).collect();
        kept.sort();
        assert_eq!(kept, ["0.6.0", "0.6.1", "0.6.2"]);
    }

    #[test]
    fn update_failures_pick_the_right_way_out() {
        let s = |x: &str| x.to_string();
        // only a refused API or an archive this machine can't unpack is worth the install script
        assert_eq!(exit_code(&Fail::Refused(s("rate limited"))), TRY_SCRIPT);
        assert_eq!(exit_code(&Fail::Unpack(s("no tar"))), TRY_SCRIPT);
        for f in [Fail::Offline(s("offline")), Fail::Bad(s("says 0.6.9")), Fail::Backup(s("disk full")), Fail::Swap(s("in use"), std::io::ErrorKind::Other)] {
            assert_ne!(exit_code(&f), TRY_SCRIPT, "{f}");
            assert_ne!(exit_code(&f), 0);
        }
        // a folder that isn't writable gets its own code (and a hint), not a reinstall somewhere else
        let denied = Fail::Swap(s("couldn't move the current oriel aside"), std::io::ErrorKind::PermissionDenied);
        assert!(denied.denied() && !denied.script_may_help());
        assert_eq!(exit_code(&denied), 3);
        // Linux's "…/oriel (deleted)" means the file now at the plain path; a path that's gone is refused
        let (d, exe) = scratch("deleted");
        fake(&exe, "0.7.0");
        assert_eq!(live_path(&PathBuf::from(format!("{} (deleted)", exe.display()))), Some(exe.clone()));
        assert_eq!(live_path(&exe), Some(exe.clone()));
        assert_eq!(live_path(&d.join("gone")), None);
        assert_ne!(unique("oriel-update"), unique("oriel-update"), "two updates never share a temp folder");
    }

    /// A real update and rollback, on a scratch copy (downloads the latest release):
    /// `cargo build; cargo test update_live_install -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn update_live_install() {
        let (_d, exe) = scratch("live");
        // "the installed version": the dev build (it says VERSION)
        let dev = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join(exe_name());
        std::fs::copy(&dev, &exe).unwrap();
        let rs = releases().unwrap();
        let latest = available(&rs).unwrap_or_else(|| rs[0].clone());
        install_to(&latest, &exe, VERSION, &|s| println!(":: {s}")).unwrap();
        assert_eq!(version_of(&exe).unwrap(), format!("oriel {}", latest.version), "swapped in");
        assert!(backups().iter().any(|(v, _)| v == VERSION), "the old one is kept");
        // and back, when the release is newer than the dev build
        if newer(&latest.version, VERSION) {
            assert_eq!(rollback_to(&exe, &latest.version).unwrap(), VERSION);
            assert_eq!(version_of(&exe).unwrap(), format!("oriel {VERSION}"));
        } else {
            assert!(rollback_to(&exe, &latest.version).is_err(), "nothing older to go back to");
        }
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
