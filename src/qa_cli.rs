//! QA: the command line and its helpers, exercised for real.
//!
//! * The built binary runs as a child process with non-interactive arguments only (`--version`, `--help`,
//!   `--config`, `changelog`, `update --no-fallback`, `rollback`, `report`, `usage-sink`, `--mcp-approve`,
//!   `mcp-lead`), always from a copy in `target/test-scratch/qa-cli/`, with `ORIEL_DATA_DIR`, `TEMP` and `TMP`
//!   pointed into scratch. "Offline" runs point `ALL_PROXY` at a dead loopback port, so nothing leaves the machine.
//! * The self-update install / rollback paths (`update::install_to`, `update::rollback_to`) run in-process on
//!   scratch copies, with a release archive built by the test and served from a loopback HTTP server.
//!
//!     cargo build; cargo test qa_cli
//!     cargo test qa_cli -- --ignored        (the read-only GitHub checks, and the known failures)

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const V: &str = env!("CARGO_PKG_VERSION");
const OLD_MARK: &[u8] = b"\nQA-OLD-BUILD";
const NEW_MARK: &[u8] = b"\nQA-NEW-BUILD";

fn exe_name() -> &'static str {
    if cfg!(windows) { "oriel.exe" } else { "oriel" }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// `target/test-scratch/qa-cli/<name>`, emptied.
fn scratch(name: &str) -> PathBuf {
    let d = std::path::absolute(format!("target/test-scratch/qa-cli/{name}")).unwrap();
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The binary `cargo build` made, copied once into scratch so a parallel build can't swap it mid-test.
fn bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let built = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join(exe_name());
        assert!(built.is_file(), "no {} - run `cargo build` before these tests", built.display());
        let copy = scratch("bin").join(exe_name());
        std::fs::copy(&built, &copy).unwrap();
        let out = Command::new(&copy).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("oriel {V}"), "stale {} - run `cargo build`", built.display());
        copy
    })
    .clone()
}

fn bin_bytes() -> Vec<u8> {
    std::fs::read(bin()).unwrap()
}

/// A private copy of the binary at `<dir>/oriel.exe`, optionally with a marker appended (the loader ignores
/// trailing bytes, so it still runs, and the marker tells builds apart after a swap).
fn exe_copy(dir: &Path, mark: &[u8]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(exe_name());
    let mut b = bin_bytes();
    b.extend_from_slice(mark);
    std::fs::write(&p, b).unwrap();
    p
}

fn ends_with(p: &Path, mark: &[u8]) -> bool {
    std::fs::read(p).map(|b| b.ends_with(mark)).unwrap_or(false)
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    v.sort();
    v
}

enum In<'a> {
    Null,
    Bytes(&'a [u8]),
    /// a pipe that is never written to or closed (a hook that forgot to close stdin)
    Open,
}

#[derive(Debug)]
struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    took: Duration,
}

/// Run `exe args` with its data folder, TEMP and TMP inside `data`, killing it after `timeout`.
fn run(exe: &Path, args: &[&str], stdin: In, data: &Path, env: &[(&str, &str)], timeout: Duration) -> Out {
    let tmp = data.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut c = Command::new(exe);
    c.args(args)
        .env("ORIEL_DATA_DIR", data)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .env("NO_COLOR", "1")
        .env_remove("ORIEL_TASK_ID")
        .stdin(match stdin {
            In::Null => Stdio::null(),
            _ => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // no console window
    }
    let start = Instant::now();
    let mut child = c.spawn().unwrap();
    let keep = match stdin {
        In::Bytes(b) => {
            let mut si = child.stdin.take().unwrap();
            let _ = si.write_all(b);
            None
        }
        In::Open => child.stdin.take(),
        In::Null => None,
    };
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let t1 = std::thread::spawn(move || {
        let mut s = vec![];
        let _ = so.read_to_end(&mut s);
        s
    });
    let t2 = std::thread::spawn(move || {
        let mut s = vec![];
        let _ = se.read_to_end(&mut s);
        s
    });
    let code = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st.code();
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("oriel {args:?} was still running after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let took = start.elapsed();
    drop(keep);
    let stdout = String::from_utf8_lossy(&t1.join().unwrap()).into_owned();
    let stderr = String::from_utf8_lossy(&t2.join().unwrap()).into_owned();
    Out { code, stdout, stderr, took }
}

/// Environment that makes every HTTP request fail fast, locally: a proxy on a loopback port nobody listens on.
const OFFLINE: &[(&str, &str)] = &[("ALL_PROXY", "http://127.0.0.1:9"), ("HTTPS_PROXY", "http://127.0.0.1:9"), ("HTTP_PROXY", "http://127.0.0.1:9"), ("NO_PROXY", "")];

fn jsonl(s: &str) -> Vec<Value> {
    s.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?} ({e})"))).collect()
}

// ====================================================================== plain flags

#[test]
fn qa_cli_version_flags() {
    let d = scratch("version");
    for flag in ["--version", "-V"] {
        let o = run(&bin(), &[flag], In::Null, &d, &[], secs(30));
        assert_eq!(o.code, Some(0), "{flag}: {o:?}");
        assert_eq!(o.stdout, format!("oriel {V}\n"), "{flag}");
        assert!(o.stderr.is_empty(), "{flag}: {o:?}");
    }
    assert_eq!(names(&d), vec!["tmp"], "--version wrote into the data folder");
}

#[test]
fn qa_cli_help_names_real_commands_and_apps() {
    let d = scratch("help");
    let mut texts = vec![];
    for flag in ["-h", "--help"] {
        let o = run(&bin(), &[flag], In::Null, &d, &[], secs(30));
        assert_eq!(o.code, Some(0), "{flag}: {o:?}");
        assert!(o.stderr.is_empty(), "{flag}: {o:?}");
        assert!(o.stdout.starts_with(&format!("oriel {V} ")), "{flag}: {}", o.stdout);
        for want in ["oriel --config", "oriel update", "oriel rollback", "oriel changelog", "oriel --tour", "oriel --version", "F10 help"] {
            assert!(o.stdout.contains(want), "{flag} doesn't mention {want}:\n{}", o.stdout);
        }
        texts.push(o.stdout);
    }
    assert_eq!(texts[0], texts[1], "-h and --help differ");
    // every app the help offers as `oriel <app>` is one the app actually opens (not the home-screen fallback)
    let line = texts[0].lines().find(|l| l.contains("open straight into an app:")).expect("no app list in --help");
    let apps: Vec<&str> = line.split(':').nth(1).unwrap().split_whitespace().collect();
    assert!(apps.len() >= 10, "{apps:?}");
    for a in &apps {
        let known = crate::app::SIDEBAR.iter().any(|s| s.0 == *a) || crate::panes::APPS.iter().any(|s| s.0 == *a);
        assert!(known, "--help offers `oriel {a}`, which only opens the home screen");
    }
    // and every sidebar app can be opened that way
    for s in crate::app::SIDEBAR {
        assert!(apps.contains(&s.0) || s.0 == "alerts", "`oriel {}` works but --help doesn't list it", s.0);
    }
    assert_eq!(names(&d), vec!["tmp"], "--help wrote into the data folder");
}

#[test]
fn qa_cli_config_prints_the_config_path() {
    let d = scratch("config");
    let o = run(&bin(), &["--config"], In::Null, &d, &[], secs(30));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert!(o.stderr.is_empty(), "{o:?}");
    assert_eq!(o.stdout.lines().count(), 1, "{o:?}");
    let p = PathBuf::from(o.stdout.trim());
    assert!(p.is_absolute(), "{}", p.display());
    // run() gives the binary a profile (ORIEL_DATA_DIR = d), and a profile has its own config.toml: the one its
    // first-run setup saves to, never the main one
    assert_eq!(p, d.join("config.toml"), "--config names the profile's config");
    assert!(crate::config::path().ends_with(Path::new("oriel").join("config.toml")), "without a profile: the main one");
    assert_eq!(names(&d), vec!["tmp"], "--config wrote into the data folder");
}

/// One mistyped field in config.toml silently throws away every other setting (prefix, theme, AI keys…):
/// `config::load` parses all-or-nothing and falls back to defaults without a word, and `oriel --config` only
/// prints the path, so there is nowhere to find out why settings "reset".
#[test]
#[ignore = "fails: one bad field in config.toml silently resets every setting to defaults (config.rs:182)"]
fn qa_cli_config_one_bad_field_keeps_the_rest() {
    let good = "prefix = \"ctrl+b\"\ntheme = \"nord\"\n";
    let c: crate::config::Config = toml::from_str(good).unwrap();
    assert_eq!(c.prefix, "ctrl+b");
    // the same file with one value of the wrong type (a very common hand-edit slip)
    let typo = format!("{good}plain_icons = \"yes\"\n");
    let parsed = toml::from_str::<crate::config::Config>(&typo).ok();
    // what load() does with it: `.ok()...unwrap_or_default()`
    let loaded = parsed.unwrap_or_default();
    assert_eq!(loaded.prefix, "ctrl+b", "a typo in plain_icons also reset the prefix key (and everything else)");
}

// ====================================================================== changelog / update / rollback via the binary

#[test]
fn qa_cli_changelog_offline_reports_the_error() {
    let d = scratch("changelog-offline");
    for alias in ["changelog", "--changelog", "whats-new"] {
        let o = run(&bin(), &[alias], In::Null, &d, OFFLINE, secs(60));
        assert!(o.stdout.is_empty(), "{alias}: {o:?}");
        assert!(o.stderr.contains("couldn't reach GitHub"), "{alias}: {o:?}");
        assert!(!d.join("update").join("checked.json").exists(), "{alias}: a failed check was cached");
    }
}

#[test]
#[ignore = "fails: `oriel changelog` exits 0 when it couldn't reach GitHub (main.rs:64 prints the error, then returns Ok)"]
fn qa_cli_changelog_offline_exits_nonzero() {
    let d = scratch("changelog-offline-code");
    let o = run(&bin(), &["changelog"], In::Null, &d, OFFLINE, secs(60));
    assert!(o.stderr.contains("couldn't reach GitHub"), "{o:?}");
    assert_ne!(o.code, Some(0), "changelog failed but exited 0: {o:?}");
}

/// Read-only against GitHub.
#[test]
#[ignore = "network: reads the GitHub releases list (read-only)"]
fn qa_cli_changelog_live() {
    let d = scratch("changelog-live");
    let o = run(&bin(), &["changelog"], In::Null, &d, &[], secs(90));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert!(o.stderr.is_empty(), "{o:?}");
    assert!(o.stdout.starts_with("## "), "{}", o.stdout);
    assert!(!o.stdout.contains("**Full Changelog**"), "{}", o.stdout);
    assert!(o.stdout.contains(&format!("## {V}\n")), "this build's own version ({V}) has no release notes:\n{}", o.stdout);
    let cached: Value = serde_json::from_str(&std::fs::read_to_string(d.join("update").join("checked.json")).unwrap()).unwrap();
    assert!(cached["releases"].as_array().is_some_and(|r| !r.is_empty()), "the check wasn't cached in the data folder");
}

#[test]
fn qa_cli_update_offline_no_fallback_fails_cleanly() {
    let d = scratch("update-offline");
    let exe = exe_copy(&d.join("app"), b"");
    let before = std::fs::read(&exe).unwrap();
    for cmd in ["update", "--update"] {
        let o = run(&exe, &[cmd, "--no-fallback"], In::Null, &d.join("data"), OFFLINE, secs(60));
        assert_eq!(o.code, Some(1), "{cmd}: {o:?}");
        assert!(o.stdout.contains(&format!("oriel {V}: checking for updates")), "{cmd}: {o:?}");
        assert!(o.stderr.contains("couldn't reach GitHub"), "{cmd}: {o:?}");
        assert!(!o.stdout.contains("install script"), "{cmd}: --no-fallback still tried the install script: {o:?}");
    }
    assert_eq!(std::fs::read(&exe).unwrap(), before, "the binary changed");
    assert_eq!(names(&d.join("app")), vec![exe_name()], "something was moved aside");
    assert!(!d.join("data").join("update").join("previous").exists(), "a backup was made without an update");
}

/// Read-only when up to date; if GitHub has something newer it updates the scratch copy (never the real one).
#[test]
#[ignore = "network: asks GitHub for the newest release (read-only when up to date)"]
fn qa_cli_update_live_from_a_copy() {
    let d = scratch("update-live");
    let exe = exe_copy(&d.join("app"), OLD_MARK);
    let built = std::fs::read(bin()).unwrap();
    let o = run(&exe, &["update", "--no-fallback"], In::Null, &d.join("data"), &[], secs(300));
    println!("{}{}", o.stdout, o.stderr);
    assert_eq!(o.code, Some(0), "{o:?}");
    if o.stdout.contains("you have the newest version.") {
        assert!(ends_with(&exe, OLD_MARK), "up to date, yet the binary changed");
        assert_eq!(names(&d.join("app")), vec![exe_name()]);
    } else {
        assert!(o.stdout.contains(":: updated to"), "{o:?}");
        assert!(!ends_with(&exe, OLD_MARK), "said updated, but the old binary is still there");
        assert!(names(&d.join("data").join("update").join("previous")).contains(&format!("oriel-{V}.exe")));
    }
    assert_eq!(std::fs::read(bin()).unwrap(), built, "the built binary was touched");
    assert!(names(&d.join("data").join("tmp")).is_empty(), "temp files left behind: {:?}", names(&d.join("data").join("tmp")));
}

#[test]
fn qa_cli_rollback_with_nothing_kept() {
    let d = scratch("rollback-none");
    let exe = exe_copy(&d.join("app"), b"");
    let before = std::fs::read(&exe).unwrap();
    for cmd in ["rollback", "--rollback"] {
        let o = run(&exe, &[cmd], In::Null, &d.join("data"), &[], secs(30));
        assert_eq!(o.code, Some(1), "{cmd}: {o:?}");
        assert!(o.stdout.is_empty(), "{cmd}: {o:?}");
        assert!(o.stderr.contains("no earlier version kept yet"), "{cmd}: {o:?}");
    }
    assert_eq!(std::fs::read(&exe).unwrap(), before);
    assert_eq!(names(&d.join("app")), vec![exe_name()]);
}

/// A real rollback by the binary, on a scratch copy: the kept version comes back, the running one steps aside and
/// is kept in turn, the kept file itself survives.
#[test]
fn qa_cli_rollback_puts_the_kept_version_back() {
    let d = scratch("rollback-kept");
    let data = d.join("data");
    let exe = exe_copy(&d.join("app"), NEW_MARK);
    let prev = data.join("update").join("previous");
    let kept = exe_copy(&prev, OLD_MARK);
    std::fs::rename(&kept, prev.join("oriel-0.0.1.exe")).unwrap();
    let o = run(&exe, &["rollback"], In::Null, &data, &[], secs(60));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stdout.trim(), ":: back to oriel 0.0.1 — start oriel again to use it", "{o:?}");
    assert!(ends_with(&exe, OLD_MARK), "the kept build isn't in place");
    let app = names(&d.join("app"));
    assert_eq!(app.len(), 2, "{app:?}");
    assert!(app[1].starts_with("oriel.exe.old-"), "the running binary wasn't moved aside: {app:?}");
    assert!(ends_with(&d.join("app").join(&app[1]), NEW_MARK));
    assert_eq!(names(&prev), vec!["oriel-0.0.1.exe".to_string(), format!("oriel-{V}.exe")], "both versions stay kept");
    assert!(ends_with(&prev.join(format!("oriel-{V}.exe")), NEW_MARK), "the version rolled back from wasn't kept");
    assert!(ends_with(&prev.join("oriel-0.0.1.exe"), OLD_MARK), "rolling back consumed the kept file");
    let u: Value = serde_json::from_str(&std::fs::read_to_string(data.join("update").join("updated.json")).unwrap()).unwrap();
    assert_eq!((u["from"].as_str(), u["to"].as_str(), u["rollback"].as_bool()), (Some(V), Some("0.0.1"), Some(true)));
    assert!(names(&data.join("tmp")).is_empty(), "staged copy left in TEMP: {:?}", names(&data.join("tmp")));
    // what came back still runs
    let o = run(&exe, &["--version"], In::Null, &data, &[], secs(30));
    assert_eq!(o.stdout, format!("oriel {V}\n"));
}

// ====================================================================== update.rs install_to / rollback_to in-process

/// The update tests share the product's test state folder (target/test-scratch/update): one at a time.
static UPDATE_LOCK: Mutex<()> = Mutex::new(());

fn update_state() -> PathBuf {
    // what update::state_dir() is under cfg(test)
    std::path::absolute("target/test-scratch/update").unwrap()
}

fn fresh_update_state() {
    let _ = std::fs::remove_dir_all(update_state());
    std::fs::create_dir_all(update_state()).unwrap();
}

fn kept() -> Vec<String> {
    let mut v: Vec<String> = crate::update::backups().into_iter().map(|(v, _)| v).collect();
    v.sort();
    v
}

/// A release archive as the release workflow makes it (the binary at the root), built with the system tar.
fn archive(dir: &Path, files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let stage = dir.join("stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).unwrap();
    for (n, b) in files {
        std::fs::write(stage.join(n), b).unwrap();
    }
    let out = dir.join(if cfg!(windows) { "release.zip" } else { "release.tar.gz" });
    let _ = std::fs::remove_file(&out);
    let tar = if cfg!(windows) { PathBuf::from(std::env::var("SystemRoot").unwrap_or("C:\\Windows".into())).join("System32").join("tar.exe") } else { PathBuf::from("tar") };
    let mut c = Command::new(tar);
    c.args(if cfg!(windows) { ["-a", "-cf"] } else { ["-z", "-cf"] }).arg(&out).arg("-C").arg(&stage);
    for (n, _) in files {
        c.arg(n);
    }
    let r = c.output().unwrap();
    assert!(r.status.success(), "tar: {}", String::from_utf8_lossy(&r.stderr));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::read(&out).unwrap()
}

/// Serve one HTTP response on loopback (200 with `body`, or 404), for `update::install_to` to download.
fn serve_once(body: Option<Vec<u8>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let Ok((mut s, _)) = l.accept() else { return };
        let mut head = vec![];
        let mut buf = [0u8; 4096];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
        let _ = match &body {
            Some(b) => write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len()).and_then(|_| s.write_all(b)),
            None => write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        };
        let _ = s.flush();
    });
    format!("http://127.0.0.1:{port}/oriel-windows-x86_64.zip")
}

fn release(version: &str, asset: String) -> crate::update::Release {
    crate::update::Release { version: version.into(), notes: String::new(), asset, published: String::new() }
}

#[test]
fn qa_update_install_to_swaps_and_rolls_back() {
    let _g = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    fresh_update_state();
    let d = scratch("install-ok");
    let app = d.join("app");
    let exe = exe_copy(&app, OLD_MARK);
    std::fs::write(app.join("oriel.exe.old-123"), b"leftover from an earlier update").unwrap();
    let mut new = bin_bytes();
    new.extend_from_slice(NEW_MARK);
    let zip = archive(&d, &[(exe_name(), new)]);
    let said = Mutex::new(vec![]);
    let r = crate::update::install_to(&release(V, serve_once(Some(zip))), &exe, "0.0.1", &|s| said.lock().unwrap().push(s.to_string()));
    assert_eq!(r, Ok(()));
    assert_eq!(*said.lock().unwrap(), vec![format!("downloading {V}"), "unpacking".into(), "keeping this version for rollback".into(), "swapping it in".into()]);
    assert!(ends_with(&exe, NEW_MARK), "the new build isn't in place");
    let left = names(&app);
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(!left.contains(&"oriel.exe.old-123".to_string()), "the earlier leftover wasn't cleared: {left:?}");
    assert!(ends_with(&app.join(&left[1]), OLD_MARK), "the old build wasn't moved aside: {left:?}");
    assert_eq!(kept(), vec!["0.0.1"]);
    let u: Value = serde_json::from_str(&std::fs::read_to_string(update_state().join("updated.json")).unwrap()).unwrap();
    assert_eq!((u["from"].as_str(), u["to"].as_str()), (Some("0.0.1"), Some(V)));
    // the first start of the new version says so, once
    assert_eq!(crate::update::just_updated(), Some(("0.0.1".to_string(), V.to_string(), false)));
    assert_eq!(crate::update::just_updated(), None);
    // and back
    assert_eq!(crate::update::rollback_to(&exe, V), Ok("0.0.1".to_string()));
    assert!(ends_with(&exe, OLD_MARK), "rollback didn't put the old build back");
    assert_eq!(kept(), vec!["0.0.1".to_string(), V.to_string()]);
    let back = crate::update::backups().into_iter().find(|(v, _)| v == V).unwrap().1;
    assert!(ends_with(&back, NEW_MARK), "the version rolled back from wasn't kept");
    let left = names(&app);
    assert_eq!(left.len(), 2, "one moved-aside file at a time: {left:?}");
}

#[test]
fn qa_update_install_to_refuses_bad_downloads() {
    let _g = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    fresh_update_state();
    let d = scratch("install-bad");
    let app = d.join("app");
    let exe = exe_copy(&app, OLD_MARK);
    let before = std::fs::read(&exe).unwrap();
    let good = {
        let mut b = bin_bytes();
        b.extend_from_slice(NEW_MARK);
        archive(&d, &[(exe_name(), b)])
    };
    let cases: Vec<(&str, crate::update::Release, &str)> = vec![
        ("no build for this machine", release("0.9.0", String::new()), "has no build for this machine"),
        ("404", release("0.9.0", serve_once(None)), "download failed"),
        ("not an archive", release("0.9.0", serve_once(Some(b"PK\x03\x04 this is not really a zip".repeat(50)))), "couldn't unpack it"),
        ("no oriel inside", release("0.9.0", serve_once(Some(archive(&d, &[("README.txt", b"hi".to_vec())])))), "had no oriel in it"),
        ("wrong version inside", release("9.9.9", serve_once(Some(good))), "not 9.9.9"),
    ];
    for (what, r, want) in cases {
        let e = crate::update::install_to(&r, &exe, "0.0.1", &|_| {}).expect_err(what);
        assert!(e.to_string().contains(want), "{what}: {e}");
        assert_eq!(std::fs::read(&exe).unwrap(), before, "{what}: the binary changed");
        assert_eq!(names(&app), vec![exe_name()], "{what}: something was moved aside");
        assert!(kept().is_empty(), "{what}: a backup was made for an update that never happened: {:?}", kept());
        assert!(!update_state().join("updated.json").exists(), "{what}: recorded as updated");
    }
}

/// A kept version that won't start is refused, and nothing moves.
#[test]
fn qa_update_rollback_to_refuses_a_broken_backup() {
    let _g = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    fresh_update_state();
    let d = scratch("rollback-broken");
    let exe = exe_copy(&d.join("app"), NEW_MARK);
    let prev = update_state().join("previous");
    std::fs::create_dir_all(&prev).unwrap();
    std::fs::write(prev.join(format!("oriel-0.6.2{}", if cfg!(windows) { ".exe" } else { "" })), b"MZ not a program").unwrap();
    let e = crate::update::rollback_to(&exe, V).expect_err("rolled back to a broken file");
    assert!(e.contains("won't start"), "{e}");
    assert!(ends_with(&exe, NEW_MARK));
    assert_eq!(names(&d.join("app")), vec![exe_name()]);
    assert_eq!(kept(), vec!["0.6.2"]);
}

/// `oriel rollback` twice: the second one should go further back (or refuse), never forward. It goes forward:
/// the first rollback keeps the version it left (0.7.0), and rollback_to picks the *highest* kept version other
/// than the running one, so the second "rollback" reinstalls 0.7.0 and prints ":: back to oriel 0.7.0".
#[test]
#[ignore = "fails: a second `oriel rollback` goes forward to the version just rolled back from (update.rs:260-262)"]
fn qa_update_rollback_twice_never_goes_forward() {
    let _g = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    fresh_update_state();
    let d = scratch("rollback-twice");
    let exe = exe_copy(&d.join("app"), b"\nQA-0.7.0");
    let prev = update_state().join("previous");
    for v in ["0.6.1", "0.6.2"] {
        let p = exe_copy(&prev, format!("\nQA-{v}").as_bytes());
        std::fs::rename(&p, prev.join(format!("oriel-{v}.exe"))).unwrap();
    }
    assert_eq!(crate::update::rollback_to(&exe, "0.7.0"), Ok("0.6.2".to_string()));
    let second = crate::update::rollback_to(&exe, "0.6.2");
    assert!(
        second.as_ref().map_or(true, |v| crate::update::parse(v) < crate::update::parse("0.6.2")),
        "rolling back from 0.6.2 went to {second:?}"
    );
}

/// "keeping this version for rollback" should keep it. back_up_current prunes to the three *highest* version
/// numbers, so the copy of the version you're updating from is deleted right after it's made whenever three
/// newer ones are already kept (e.g. after installing an older build by hand to bisect a regression).
#[test]
#[ignore = "fails: the backup of the version being updated from is pruned at once if 3 newer are kept (update.rs:158-162)"]
fn qa_update_install_keeps_the_version_it_replaced() {
    let _g = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    fresh_update_state();
    let d = scratch("install-prune");
    let exe = exe_copy(&d.join("app"), OLD_MARK);
    let prev = update_state().join("previous");
    for v in ["0.6.0", "0.6.1", "0.6.2"] {
        let p = exe_copy(&prev, format!("\nQA-{v}").as_bytes());
        std::fs::rename(&p, prev.join(format!("oriel-{v}.exe"))).unwrap();
    }
    let mut new = bin_bytes();
    new.extend_from_slice(NEW_MARK);
    let zip = archive(&d, &[(exe_name(), new)]);
    crate::update::install_to(&release(V, serve_once(Some(zip))), &exe, "0.5.0", &|_| {}).unwrap();
    assert!(kept().contains(&"0.5.0".to_string()), "the version just replaced isn't kept: {:?}", kept());
}

// ====================================================================== report (agent hooks)

fn status(data: &Path, id: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(data.join("agents").join("status").join(format!("{id}.json"))).ok()?).ok()
}

#[test]
fn qa_cli_report_writes_the_hook_status() {
    let d = scratch("report-ok");
    let quiet = |o: &Out| assert!(o.code == Some(0) && o.stdout.is_empty() && o.stderr.is_empty(), "report must be silent and exit 0: {o:?}");
    let hook = json!({"hook_event_name": "PreToolUse", "session_id": "s1", "transcript_path": "C:/t/s1.jsonl", "tool_name": "Edit", "tool_input": {"file_path": "C:/w/src/app.rs"}}).to_string();
    quiet(&run(&bin(), &["report", "--task", "k3f9", "--state", "running"], In::Bytes(hook.as_bytes()), &d, &[], secs(30)));
    let s = status(&d, "k3f9").expect("no status file");
    assert_eq!((s["state"].as_str(), s["last_tool"].as_str(), s["session_id"].as_str()), (Some("running"), Some("Edit app.rs"), Some("s1")), "{s}");
    assert!(s["ts"].as_i64().unwrap_or(0) > 1_700_000_000, "{s}");
    // a question: blocked with its message, the session id stays
    let q = json!({"hook_event_name": "Notification", "message": "Claude needs your permission to use Bash"}).to_string();
    quiet(&run(&bin(), &["report", "--state", "blocked", "--task", "k3f9"], In::Bytes(q.as_bytes()), &d, &[], secs(30)));
    let s = status(&d, "k3f9").unwrap();
    assert_eq!((s["state"].as_str(), s["message"].as_str(), s["session_id"].as_str()), (Some("blocked"), Some("Claude needs your permission to use Bash"), Some("s1")), "{s}");
    // "waiting for your input" is idle, not blocked
    let idle = json!({"notification_type": "idle_prompt", "message": "Claude is waiting for your input"}).to_string();
    quiet(&run(&bin(), &["report", "--task", "k3f9", "--state", "blocked"], In::Bytes(idle.as_bytes()), &d, &[], secs(30)));
    assert_eq!(status(&d, "k3f9").unwrap()["state"], "idle");
    // the task id can come from the environment (terminal panes set ORIEL_TASK_ID)
    quiet(&run(&bin(), &["report", "--state", "session"], In::Null, &d, &[("ORIEL_TASK_ID", "envtask")], secs(30)));
    assert_eq!(status(&d, "envtask").expect("ORIEL_TASK_ID ignored")["state"], "running");
    // garbage on stdin still records the state
    quiet(&run(&bin(), &["report", "--task", "g1", "--state", "running"], In::Bytes(b"{not json at all"), &d, &[], secs(30)));
    assert_eq!(status(&d, "g1").expect("garbage stdin dropped the report")["state"], "running");
}

#[test]
fn qa_cli_report_bad_input_is_silent_and_harmless() {
    let d = scratch("report-bad");
    let data = d.join("data");
    let bad: &[&[&str]] = &[
        &["report"],
        &["report", "--task"],
        &["report", "--state"],
        &["report", "--task", "abc"],
        &["report", "--state", "running"],
        &["report", "--task", "abc", "--state"],
        &["report", "--task", "abc", "--state", "bogus"],
        &["report", "--task", "abc", "--state", "RUNNING"],
        &["report", "--task", "../../evil", "--state", "running"],
        &["report", "--task", "..", "--state", "running"],
        &["report", "--task", "a/b", "--state", "running"],
        &["report", "--task", "a\\b", "--state", "running"],
        &["report", "--task", "C:evil", "--state", "running"],
        &["report", "--task", "", "--state", "running"],
        &["report", "--task", "ab c", "--state", "running"],
    ];
    for args in bad {
        let o = run(&bin(), args, In::Bytes(b"{\"session_id\":\"x\"}"), &data, &[], secs(30));
        assert!(o.code == Some(0) && o.stdout.is_empty() && o.stderr.is_empty(), "{args:?}: {o:?}");
    }
    assert!(names(&data.join("agents").join("status")).is_empty(), "bad input wrote a status: {:?}", names(&data.join("agents").join("status")));
    let mut outside = names(&d);
    outside.retain(|n| n != "data");
    assert!(outside.is_empty(), "wrote outside the data folder: {outside:?}");
    assert!(!data.join("agents").join("evil.json").exists() && !data.join("evil.json").exists());
}

/// A hook that never closes stdin must not hang the agent: report gives up on stdin after a moment.
#[test]
fn qa_cli_report_never_hangs_on_an_open_stdin() {
    let d = scratch("report-open-stdin");
    let o = run(&bin(), &["report", "--task", "t1", "--state", "running"], In::Open, &d, &[], secs(20));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert!(o.took < secs(6), "report took {:?} with an open stdin", o.took);
    assert_eq!(status(&d, "t1").expect("no status")["state"], "running");
}

// ====================================================================== usage-sink (Claude Code's statusLine)

const STATUS_JSON: &str = r#"{"session_id":"s","transcript_path":"C:/secret/path.jsonl","cwd":"C:/private/repo","model":{"id":"claude-opus-5-5","display_name":"Opus 5.5"},"cost":{"total_cost_usd":0.4213,"total_lines_added":3},"rate_limits":{"five_hour":{"used_percentage":34.2,"resets_at":2000000000},"seven_day":{"used_percentage":12,"resets_at":2000300000}}}"#;

#[test]
fn qa_cli_usage_sink_bad_input_prints_a_fallback() {
    let d = scratch("sink-bad");
    let inputs: [(&str, Option<&[u8]>); 5] = [("no stdin", None), ("empty", Some(b"")), ("not json", Some(b"{nope")), ("invalid utf-8", Some(&[0xff, 0xfe, 0x00, 0x7b])), ("half a message", Some(&STATUS_JSON.as_bytes()[..60]))];
    for (what, input) in inputs {
        let o = run(&bin(), &["usage-sink"], input.map_or(In::Null, In::Bytes), &d, &[], secs(30));
        assert_eq!(o.code, Some(0), "{what}: {o:?}");
        assert_eq!(o.stdout, "oriel: no status\n", "{what}: {o:?}");
        assert!(o.stderr.is_empty(), "{what}: {o:?}");
    }
    assert!(!d.join("usage").join("claude.json").exists(), "junk was saved as usage");
}

#[test]
fn qa_cli_usage_sink_keeps_numbers_only() {
    let d = scratch("sink-ok");
    let o = run(&bin(), &["usage-sink"], In::Bytes(STATUS_JSON.as_bytes()), &d, &[], secs(30));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stdout, "5h 34% · wk 12% · $0.42\n");
    let saved = std::fs::read_to_string(d.join("usage").join("claude.json")).expect("nothing saved");
    assert!(!saved.contains("secret") && !saved.contains("private") && !saved.contains("transcript_path") && !saved.contains("session_id"), "{saved}");
    let v: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(v["rate_limits"]["five_hour"]["used_percentage"], 34.2);
    assert_eq!(v["model"]["display_name"], "Opus 5.5");
    // no rate limits, no cost (e.g. an API-key session): the model name
    let o = run(&bin(), &["usage-sink"], In::Bytes(br#"{"model":{"display_name":"Sonnet 5"}}"#), &d, &[], secs(30));
    assert_eq!(o.stdout, "Sonnet 5\n");
}

#[test]
fn qa_cli_usage_sink_then_chains_a_command() {
    let d = scratch("sink-then");
    let o = run(&bin(), &["usage-sink", "--then", "echo", "chained-ok"], In::Bytes(STATUS_JSON.as_bytes()), &d, &[], secs(30));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stdout.trim(), "chained-ok", "{o:?}");
    assert!(d.join("usage").join("claude.json").exists(), "chaining skipped the save");
    // the chained command gets the same JSON on its stdin
    let o = run(&bin(), &["usage-sink", "--then", "cat"], In::Bytes(STATUS_JSON.as_bytes()), &d, &[], secs(30));
    assert_eq!(o.stdout.trim(), STATUS_JSON, "{o:?}");
    // `--then` with nothing after it is oriel's own line
    let o = run(&bin(), &["usage-sink", "--then"], In::Bytes(STATUS_JSON.as_bytes()), &d, &[], secs(30));
    assert_eq!(o.stdout, "5h 34% · wk 12% · $0.42\n");
}

/// The chained statusLine is the user's old one (the token saver sets it up). When it's broken (script moved,
/// command gone) Claude Code shows an empty status line: its empty output is printed instead of oriel's.
#[test]
#[ignore = "fails: a failing `usage-sink --then` command blanks the status line instead of falling back (limits.rs:162, 216)"]
fn qa_cli_usage_sink_broken_then_falls_back() {
    let d = scratch("sink-then-broken");
    let o = run(&bin(), &["usage-sink", "--then", "exit", "3"], In::Bytes(STATUS_JSON.as_bytes()), &d, &[], secs(30));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.stdout.trim(), "5h 34% · wk 12% · $0.42", "a broken chained command left the status line blank: {o:?}");
}

/// A status update without `rate_limits` (a session before its first reply, an API-key session running next to
/// a subscription one) replaces the whole saved file, so the last known 5-hour / weekly numbers vanish from the
/// ais app and the event center until the next update that has them. `updated` already dates the numbers, so
/// keeping the old ones would be safe.
#[test]
#[ignore = "fails: a statusLine update without rate_limits erases the saved plan limits (limits.rs:143-148)"]
fn qa_cli_usage_sink_keeps_the_last_limits() {
    let d = scratch("sink-keep-limits");
    let file = d.join("usage").join("claude.json");
    run(&bin(), &["usage-sink"], In::Bytes(STATUS_JSON.as_bytes()), &d, &[], secs(30));
    assert!(std::fs::read_to_string(&file).unwrap().contains("five_hour"));
    let o = run(&bin(), &["usage-sink"], In::Bytes(br#"{"model":{"id":"claude-opus-5-5","display_name":"Opus 5.5"},"cost":{"total_cost_usd":0.01}}"#), &d, &[], secs(30));
    assert_eq!(o.code, Some(0), "{o:?}");
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(saved.contains("five_hour"), "the last known limits were erased by an update without them:\n{saved}");
}

/// `report` gives up on a stdin that never closes (read_stdin_quick); `usage-sink` reads it with no limit, so a
/// statusLine host that keeps the pipe open, or a person running it by hand, hangs it for good.
#[test]
#[ignore = "fails: `usage-sink` blocks forever on a stdin that is never closed (limits.rs:155)"]
fn qa_cli_usage_sink_never_hangs_on_an_open_stdin() {
    let d = scratch("sink-open-stdin");
    let o = run(&bin(), &["usage-sink"], In::Open, &d, &[], secs(8));
    assert_eq!(o.code, Some(0), "{o:?}");
}

// ====================================================================== --mcp-approve (Claude Code's permission prompt)

fn rpc(lines: &[Value]) -> Vec<u8> {
    let mut s: String = lines.iter().map(|v| v.to_string() + "\n").collect();
    s.push('\n');
    s.into_bytes()
}

fn with_raw(mut body: Vec<u8>, at: usize, raw: &str) -> Vec<u8> {
    // splice a raw (non-JSON) line in after the `at`-th line
    let mut pos = 0;
    for _ in 0..at {
        pos += body[pos..].iter().position(|b| *b == b'\n').unwrap() + 1;
    }
    let tail = body.split_off(pos);
    body.extend_from_slice(raw.as_bytes());
    body.push(b'\n');
    body.extend(tail);
    body
}

#[test]
fn qa_cli_mcp_approve_over_stdio_to_a_live_broker() {
    use crate::panes::chat::approve::{Broker, Decision};
    use crate::panes::chat::providers::Ev;
    let d = scratch("mcp-approve");
    let asks: Arc<Mutex<Vec<String>>> = Arc::default();
    let a2 = asks.clone();
    let send: Arc<dyn Fn(Ev) + Send + Sync> = Arc::new(move |ev| {
        if let Ev::Ask(a) = ev {
            a2.lock().unwrap().push(a.rule.split('(').next().unwrap_or("").to_string());
            let _ = a.reply.send(if a.rule.starts_with("Bash") { Decision::Allow } else { Decision::Deny });
        }
    });
    let b = Broker::start(PathBuf::from("/w"), Arc::new(Mutex::new("ask".into())), send).unwrap();
    let input = rpc(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 99}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "approve", "arguments": {"tool_name": "Bash", "input": {"command": "cargo test"}}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "approve", "arguments": {"tool_name": "Write", "input": {"file_path": "/w/a.txt", "content": "hi"}}}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": "seven", "method": "tools/call", "params": {"name": "approve"}}),
    ]);
    let input = with_raw(input, 3, "{this is not json");
    let o = run(&bin(), &["--mcp-approve", &b.port.to_string(), &b.token], In::Bytes(&input), &d, &[], secs(60));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert!(o.stderr.is_empty(), "{o:?}");
    let r = jsonl(&o.stdout);
    let ids: Vec<Value> = r.iter().map(|v| v["id"].clone()).collect();
    assert_eq!(ids, vec![json!(1), json!(2), json!(3), json!(4), json!(5), json!(6), json!("seven")], "{r:?}");
    assert!(r.iter().all(|v| v["jsonrpc"] == "2.0"), "{r:?}");
    assert_eq!(r[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(r[0]["result"]["serverInfo"], json!({"name": "oriel", "version": V}));
    assert_eq!(r[1]["result"]["tools"][0]["name"], "approve");
    assert_eq!(r[2]["error"]["code"], -32601);
    let ans = |i: usize| serde_json::from_str::<Value>(r[i]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(ans(3)["behavior"], "allow");
    assert_eq!(ans(3)["updatedInput"]["command"], "cargo test");
    assert_eq!(ans(4)["behavior"], "deny");
    assert_eq!(r[5]["result"], json!({}));
    assert_eq!(ans(6)["behavior"], "deny", "a call with no arguments");
    assert_eq!(*asks.lock().unwrap(), vec!["Bash".to_string(), "Write".into(), "tool".into()]);
    drop(b);
}

#[test]
fn qa_cli_mcp_approve_denies_when_it_cant_ask() {
    use crate::panes::chat::approve::Broker;
    use crate::panes::chat::providers::Ev;
    let d = scratch("mcp-approve-deny");
    let call = rpc(&[json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "approve", "arguments": {"tool_name": "Bash", "input": {"command": "rm -rf /"}}}})]);
    let deny_msg = |o: &Out| -> String {
        let r = jsonl(&o.stdout);
        assert_eq!(r.len(), 1, "{o:?}");
        let a: Value = serde_json::from_str(r[0]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(a["behavior"], "deny", "{a}");
        a["message"].as_str().unwrap_or("").to_string()
    };
    // no oriel on the other end: missing, junk and closed ports
    for args in [vec!["--mcp-approve"], vec!["--mcp-approve", "not-a-port", "x"], vec!["--mcp-approve", "1", "x"]] {
        let o = run(&bin(), &args, In::Bytes(&call), &d, &[], secs(60));
        assert_eq!(o.code, Some(0), "{args:?}: {o:?}");
        let m = deny_msg(&o);
        assert!(m.starts_with("Couldn't ask the user"), "{args:?}: {m}");
    }
    // a live oriel, the wrong token: refused without asking anyone
    let asked = Arc::new(Mutex::new(0));
    let a2 = asked.clone();
    let b = Broker::start(PathBuf::from("/w"), Arc::new(Mutex::new("ask".into())), Arc::new(move |_ev: Ev| *a2.lock().unwrap() += 1)).unwrap();
    let o = run(&bin(), &["--mcp-approve", &b.port.to_string(), "wrong-token"], In::Bytes(&call), &d, &[], secs(60));
    assert_eq!(deny_msg(&o), "bad token");
    assert_eq!(*asked.lock().unwrap(), 0, "a wrong token reached the chat");
}

/// JSON-RPC 2.0: a line that isn't JSON gets a -32700 parse error (id null). Both MCP servers skip it silently,
/// so a client that sent a malformed request waits for an answer that never comes.
#[test]
#[ignore = "fails: --mcp-approve and mcp-lead drop unparsable lines without a -32700 reply (approve.rs:198, mcp.rs:169)"]
fn qa_cli_mcp_servers_answer_parse_errors() {
    let d = scratch("mcp-parse-error");
    for args in [vec!["--mcp-approve", "1", "x"], vec!["mcp-lead", "1", "x"]] {
        let o = run(&bin(), &args, In::Bytes(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\n"), &d, &[], secs(30));
        let r = jsonl(&o.stdout);
        assert!(r.iter().any(|v| v["error"]["code"] == -32700), "{args:?}: no parse error reply: {o:?}");
    }
}

// ====================================================================== mcp-lead (the lead agent's tools)

/// A stand-in for the agents app's lead broker: `{"token","tool","args"}` -> `{"ok",...}`, one line each way.
fn fake_lead_broker(token: &'static str) -> (u16, Arc<Mutex<Vec<(String, Value)>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { break };
            let mut line = String::new();
            if BufReader::new(&s).read_line(&mut line).is_err() {
                continue;
            }
            let v: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
            let ans = if v["token"] != token {
                json!({"ok": false, "error": "bad token"})
            } else {
                let tool = v["tool"].as_str().unwrap_or("").to_string();
                s2.lock().unwrap().push((tool.clone(), v["args"].clone()));
                match tool.as_str() {
                    "roster" => json!({"ok": true, "text": "codex: mid"}),
                    "merge" => json!({"ok": false, "error": "conflicts in a.txt"}),
                    _ => json!({"ok": true, "text": "ok"}),
                }
            };
            let mut w = &s;
            let _ = writeln!(w, "{ans}");
        }
    });
    (port, seen)
}

#[test]
fn qa_cli_mcp_lead_over_stdio() {
    let d = scratch("mcp-lead");
    let (port, seen) = fake_lead_broker("tok123");
    let input = rpc(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "roster", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "merge", "arguments": {"id": "t1"}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "nope", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "note", "arguments": "not an object"}}),
        json!({"jsonrpc": "2.0", "id": 7, "method": "prompts/list"}),
        json!({"jsonrpc": "2.0", "method": "notifications/whatever"}),
        json!({"jsonrpc": "2.0", "id": 8, "method": "ping"}),
    ]);
    let input = with_raw(input, 2, "][ garbage");
    let o = run(&bin(), &["mcp-lead", &port.to_string(), "tok123"], In::Bytes(&input), &d, &[], secs(60));
    assert_eq!(o.code, Some(0), "{o:?}");
    assert!(o.stderr.is_empty(), "{o:?}");
    let r = jsonl(&o.stdout);
    let ids: Vec<Value> = r.iter().map(|v| v["id"].clone()).collect();
    assert_eq!(ids, (1..=8).map(|i| json!(i)).collect::<Vec<_>>(), "{r:?}");
    assert_eq!(r[0]["result"]["protocolVersion"], "2024-11-05", "no params: the default version");
    assert_eq!(r[0]["result"]["serverInfo"]["version"], V);
    assert!(r[0]["result"]["instructions"].as_str().is_some_and(|s| !s.is_empty()));
    let tools = r[1]["result"]["tools"].as_array().unwrap();
    let mut tn: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(tn.len(), 12, "{tn:?}");
    assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object" && t["description"].as_str().is_some_and(|s| s.len() > 20)), "a tool without a schema or description");
    tn.sort();
    tn.dedup();
    assert_eq!(tn.len(), 12, "duplicate tool names");
    assert_eq!(r[2]["result"]["content"][0]["text"], "codex: mid");
    assert!(r[2]["result"]["isError"].is_null());
    assert_eq!((r[3]["result"]["isError"].as_bool(), r[3]["result"]["content"][0]["text"].as_str()), (Some(true), Some("conflicts in a.txt")));
    assert_eq!((r[4]["result"]["isError"].as_bool(), r[4]["result"]["content"][0]["text"].as_str()), (Some(true), Some("no tool called nope")));
    assert_eq!(r[5]["result"]["content"][0]["text"], "ok");
    assert_eq!(r[6]["error"]["code"], -32601);
    assert_eq!(r[7]["result"], json!({}));
    assert_eq!(*seen.lock().unwrap(), vec![("roster".to_string(), json!({})), ("merge".into(), json!({"id": "t1"})), ("note".into(), json!({}))], "unknown tools must not be forwarded");
}

#[test]
fn qa_cli_mcp_lead_without_oriel_is_an_error_result() {
    let d = scratch("mcp-lead-none");
    let call = rpc(&[json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "roster", "arguments": {}}})]);
    for args in [vec!["mcp-lead"], vec!["mcp-lead", "xyz", "t"], vec!["mcp-lead", "1", "t"]] {
        let o = run(&bin(), &args, In::Bytes(&call), &d, &[], secs(60));
        assert_eq!(o.code, Some(0), "{args:?}: {o:?}");
        let r = jsonl(&o.stdout);
        assert_eq!(r.len(), 1, "{args:?}: {o:?}");
        assert_eq!(r[0]["result"]["isError"], true, "{args:?}: {r:?}");
        assert!(r[0]["result"]["content"][0]["text"].as_str().unwrap().contains("oriel isn't answering"), "{args:?}: {r:?}");
    }
    // a live broker, the wrong token
    let (port, seen) = fake_lead_broker("right");
    let o = run(&bin(), &["mcp-lead", &port.to_string(), "wrong"], In::Bytes(&call), &d, &[], secs(60));
    let r = jsonl(&o.stdout);
    assert_eq!((r[0]["result"]["isError"].as_bool(), r[0]["result"]["content"][0]["text"].as_str()), (Some(true), Some("bad token")));
    assert!(seen.lock().unwrap().is_empty());
    // and it ends with its stdin
    let o = run(&bin(), &["mcp-lead", "1", "t"], In::Bytes(b""), &d, &[], secs(30));
    assert_eq!((o.code, o.stdout.as_str()), (Some(0), ""), "{o:?}");
}
