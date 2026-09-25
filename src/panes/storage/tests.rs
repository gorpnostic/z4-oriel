//! Headless tests. Nothing here deletes, installs or uninstalls anything: under cfg(test) the pane only records
//! what it would have run (`launched`). The only files written are the fixture under target/ and the snapshots.

use super::scan::{self, Cmd, Find, How, Kind, Target};
use super::sys;
use super::*;
use crate::pane::Pane;
use crate::testkit::Kit;
use crossterm::event::KeyCode;
use std::path::PathBuf;

/// Hook the pane up to the kit without starting the real cleanup scan.
fn quiet(p: &mut Storage, k: &Kit) {
    p.started = true;
    p.out.waker = Some(crate::pane::Waker { id: 1, tx: k.tx.clone() });
}

fn target(id: &'static str, label: &str, kind: Kind, how: How, path: &str) -> Target {
    Target {
        id,
        label: label.into(),
        paths: vec![PathBuf::from(path)],
        kind,
        note: format!("why {label}"),
        how,
        find: Find::Fixed,
        open: Some(PathBuf::from(path)),
    }
}

/// A fake cleanup list: two clean rows, one root-needing row, one review row.
fn fake(p: &mut Storage) {
    p.drives = Some(vec![
        scan::Drive { name: "C:".into(), total: 952 << 30, free: 28 << 30 },
        scan::Drive { name: "D:".into(), total: 2000 << 30, free: 1200 << 30 },
    ]);
    p.targets = vec![
        target("temp", "temp files", Kind::Clean, How::Dir, "/nowhere/temp"),
        target("npm", "npm cache", Kind::Clean, How::Dir, "/nowhere/npm"),
        target("pacman", "pacman package cache", Kind::Clean, How::Term(Cmd::new("sudo", &["paccache", "-rk1"])), "/var/cache/pacman/pkg"),
        target("downloads", "Downloads", Kind::Review, How::Nothing, "/nowhere/Downloads"),
    ];
    for (id, n) in [("temp", 15u64 << 30), ("npm", 700 << 20), ("pacman", 3 << 30), ("downloads", 30 << 30)] {
        p.sizes.insert(id, (n, true));
    }
}

fn opened(k: &Kit) -> usize {
    k.actions.iter().filter(|a| matches!(a, crate::pane::Action::Open(..))).count()
}

#[test]
fn storage_cleanup_real_scan_snapshot() {
    let mut k = Kit::new();
    let mut p = Storage::new();
    k.render(&mut p, 150, 44); // starts the scan
    k.wait_wake(&mut p, 4000);
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-main.html");
    println!("{out}\n{:?}", p.subtitle());
    assert!(out.contains("what") && out.contains("why"));
    assert!(out.contains("drives"));
    assert!(!p.targets.is_empty(), "found no cleanup rows");
    assert!(p.drives.as_ref().is_some_and(|d| !d.is_empty()), "no drives");
    let side = k.render_side(&mut p, 34, 8);
    println!("{side}");
    assert!(side.contains("cleanup") && side.contains("big folders") && side.contains("installed apps"));
}

#[test]
fn storage_clean_needs_confirmation() {
    let mut k = Kit::new();
    let mut p = Storage::new();
    quiet(&mut p, &k);
    fake(&mut p);
    let out = k.render(&mut p, 150, 30);
    assert!(out.contains("15.0 GB safe to clean") || p.subtitle().unwrap().contains("safe to clean"));
    // the biggest clean row is first: temp files
    assert_eq!(p.selected().map(|i| p.targets[i].id), Some("temp"));
    k.key(&mut p, KeyCode::Char('x'));
    assert!(p.confirm.is_some());
    let out = k.render_html(&mut p, 150, 30, "target/snap/storage-confirm.html");
    println!("{out}");
    assert!(out.contains("empty temp files (15.0 GB)?"));
    // other keys do nothing while asking
    k.key(&mut p, KeyCode::Char('j'));
    k.key(&mut p, KeyCode::Enter);
    assert!(p.launched.is_empty() && opened(&k) == 0);
    k.key(&mut p, KeyCode::Esc);
    assert!(p.confirm.is_none() && p.launched.is_empty());
    assert!(k.notices().iter().any(|n| n == "cancelled"));
    // y runs it (recorded only, in tests)
    k.key(&mut p, KeyCode::Char('x'));
    k.key(&mut p, KeyCode::Char('y'));
    assert_eq!(p.launched, vec!["clean: temp".to_string()]);
}

#[test]
fn storage_root_cleanups_run_in_a_terminal() {
    let mut k = Kit::new();
    let mut p = Storage::new();
    quiet(&mut p, &k);
    fake(&mut p);
    k.key(&mut p, KeyCode::Char('j')); // pacman (3 GB) is second
    assert_eq!(p.selected().map(|i| p.targets[i].id), Some("pacman"));
    k.key(&mut p, KeyCode::Char('x'));
    let q = p.confirm.as_ref().unwrap();
    assert!(q.lines.iter().any(|l| l.contains("sudo paccache -rk1")));
    k.key(&mut p, KeyCode::Char('y'));
    assert_eq!(p.launched, vec!["term: sudo paccache -rk1".to_string()]);
}

#[test]
fn storage_review_rows_never_delete() {
    let mut k = Kit::new();
    let mut p = Storage::new();
    quiet(&mut p, &k);
    fake(&mut p);
    k.key(&mut p, KeyCode::Char('G')); // Downloads: the review row is last
    assert_eq!(p.selected().map(|i| p.targets[i].id), Some("downloads"));
    k.key(&mut p, KeyCode::Char('x'));
    assert!(p.confirm.is_none());
    assert!(k.notices().iter().any(|n| n.contains("is your data")));
    k.key(&mut p, KeyCode::Enter);
    assert_eq!(opened(&k), 1, "enter opens it in files");
    assert!(p.launched.is_empty());
}

/// target/storage-fixture: big/ (3 MiB), small/ (10 KB), a loose file, and a link back to the fixture itself.
fn fixture() -> PathBuf {
    let root = std::env::current_dir().unwrap().join("target").join("storage-fixture");
    let big = root.join("big").join("deeper");
    std::fs::create_dir_all(&big).unwrap();
    std::fs::create_dir_all(root.join("small")).unwrap();
    for i in 0..3 {
        std::fs::write(big.join(format!("{i}.bin")), vec![0u8; 1 << 20]).unwrap();
    }
    std::fs::write(root.join("small").join("a.txt"), vec![b'a'; 10_000]).unwrap();
    std::fs::write(root.join("loose.txt"), vec![b'b'; 500]).unwrap();
    let link = root.join("loop");
    if std::fs::symlink_metadata(&link).is_err() {
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&link).arg(&root).output();
        }
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&root, &link);
        }
    }
    root
}

#[test]
fn storage_walk_skips_links() {
    let root = fixture();
    let link_md = std::fs::symlink_metadata(root.join("loop"));
    assert!(link_md.as_ref().map(scan::is_link).unwrap_or(true), "the loop should be a link/junction");
    let stop = AtomicBool::new(false);
    let mut big = vec![];
    let n = scan::dir_size(&root, &stop, Some(&mut big), &mut |_| {});
    assert_eq!(n, 3 * (1 << 20) + 10_000 + 500, "walked through the link");
}

#[test]
fn storage_big_folders_drill_and_up() {
    let root = fixture();
    let mut k = Kit::new();
    let mut p = Storage::new();
    quiet(&mut p, &k);
    p.froot = root.clone();
    k.key(&mut p, KeyCode::Char('2'));
    assert_eq!(p.view, View::Folders);
    for _ in 0..50 {
        k.wait_wake(&mut p, 100);
        if p.fdone {
            break;
        }
    }
    assert!(p.fdone);
    let out = k.render_html(&mut p, 120, 24, "target/snap/storage-folders-fixture.html");
    println!("{out}");
    let order = p.order(View::Folders);
    let names: Vec<&str> = order.iter().map(|&i| p.frows[i].name.as_str()).collect();
    assert_eq!(names, vec!["big", "small", "loose.txt"], "sorted largest first, link not listed");
    assert!(out.contains("3.0 MB"));
    k.key(&mut p, KeyCode::Enter); // into big/
    assert_eq!(p.froot, root.join("big"));
    k.key(&mut p, KeyCode::Backspace); // back up, big/ stays selected
    for _ in 0..50 {
        k.wait_wake(&mut p, 100);
        if p.fdone {
            break;
        }
    }
    assert_eq!(p.froot, root);
    assert_eq!(p.selected().map(|i| p.frows[i].name.clone()), Some("big".into()));
}

#[test]
fn storage_big_folders_home_snapshot() {
    let mut k = Kit::new();
    let mut p = Storage::new();
    quiet(&mut p, &k);
    p.refresh_drives();
    k.key(&mut p, KeyCode::Char('2'));
    k.wait_wake(&mut p, 5000);
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-folders.html");
    println!("{out}\n{:?}", p.subtitle());
    assert!(!p.frows.is_empty());
    p.fstop.store(true, Ordering::Relaxed);
}

#[test]
fn storage_installed_apps_snapshot() {
    let mut k = Kit::new();
    let mut p = Storage::new();
    quiet(&mut p, &k);
    p.refresh_drives();
    k.key(&mut p, KeyCode::Char('3'));
    for _ in 0..100 {
        k.wait_wake(&mut p, 100);
        if p.apps.is_some() {
            break;
        }
    }
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-apps.html");
    println!("{out}\n{:?}", p.subtitle());
    let Some(Ok(apps)) = &p.apps else { return }; // no package manager on this box
    let n = apps.len();
    assert!(n > 0);
    let first = apps[0].name.chars().take(4).collect::<String>();
    // filter
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, &first);
    assert!(p.order(View::Apps).len() <= n);
    assert!(!p.order(View::Apps).is_empty());
    k.key(&mut p, KeyCode::Enter);
    // uninstall asks first, and esc cancels
    k.key(&mut p, KeyCode::Char('u'));
    assert!(p.confirm.as_ref().is_some_and(|c| c.question.starts_with("uninstall ")));
    k.key(&mut p, KeyCode::Esc);
    assert!(p.launched.is_empty());
}

#[test]
fn storage_install_confirm_then_terminal() {
    let mut k = Kit::new();
    let mut p = Storage::new();
    quiet(&mut p, &k);
    fake(&mut p);
    k.key(&mut p, KeyCode::Char('4'));
    assert!(p.input, "install view starts in the search box");
    k.typ(&mut p, "ripgrep");
    assert_eq!(p.query, "ripgrep");
    // don't hit the network: pretend the search came back
    p.input = false;
    p.found_q = "ripgrep".into();
    let text = "Name             Id                      Version Match        Source\n\
                --------------------------------------------------------------------\n\
                RipGrep GNU      BurntSushi.ripgrep.GNU  15.2.0  Tag: ripgrep winget\n\
                RipGrep MSVC     BurntSushi.ripgrep.MSVC 15.2.0  Tag: ripgrep winget\n";
    p.found = Some(Ok(sys::parse_winget(text)));
    k.key(&mut p, KeyCode::Char('j'));
    let out = k.render_html(&mut p, 150, 30, "target/snap/storage-install.html");
    println!("{out}");
    k.key(&mut p, KeyCode::Enter);
    assert!(p.confirm.as_ref().is_some_and(|c| c.question == "install RipGrep MSVC 15.2.0?"));
    k.key(&mut p, KeyCode::Char('y'));
    assert_eq!(p.launched, vec!["term: winget install --id BurntSushi.ripgrep.MSVC -e".to_string()]);
}

/// Hits the network: `cargo test storage_live_search -- --ignored --nocapture`.
#[test]
#[ignore]
fn storage_live_search() {
    let r = sys::search("ripgrep").unwrap();
    for f in &r {
        println!("{} | {} | {} | {} | {}", f.name, f.id, f.version, f.source, f.install.text());
    }
    assert!(!r.is_empty());
}

// ------------------------------------------------------------------ parsers

#[test]
fn storage_parse_winget() {
    let text = "   - \r   \\ \r\nName             Id                      Version Match        Source\n\
                --------------------------------------------------------------------\n\
                IndexSearch      Abyss116.IndexSearch    0.5.0   Tag: ripgrep winget\n\
                RipGrep MSVC     BurntSushi.ripgrep.MSVC 15.2.0  Tag: ripgrep winget\n";
    let r = sys::parse_winget(text);
    assert_eq!(r.len(), 2);
    assert_eq!(r[1].name, "RipGrep MSVC");
    assert_eq!(r[1].id, "BurntSushi.ripgrep.MSVC");
    assert_eq!(r[1].version, "15.2.0");
    assert_eq!(r[1].source, "winget");
}

#[test]
fn storage_parse_reg() {
    let text = r#"
HKEY_CURRENT_USER\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\abc
    DisplayName    REG_SZ    SideQuest 0.10.42
    UninstallString    REG_SZ    "C:\Users\X\AppData\Local\Programs\SideQuest\Uninstall SideQuest.exe" /currentuser
    DisplayVersion    REG_SZ    0.10.42
    Publisher    REG_SZ    Shane Harris
    EstimatedSize    REG_DWORD    0x4926b
    InstallDate    REG_SZ    20260801

HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{GUID}
    DisplayName    REG_SZ    Some Runtime
    UninstallString    REG_EXPAND_SZ    MsiExec.exe /I{1234-5678}
    SystemComponent    REG_DWORD    0x0

HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\hidden
    DisplayName    REG_SZ    Hidden Thing
    UninstallString    REG_SZ    x.exe
    SystemComponent    REG_DWORD    0x1
"#;
    let a = sys::parse_reg(text);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].name, "SideQuest 0.10.42");
    assert_eq!(a[0].size, 0x4926b * 1024);
    assert_eq!(a[0].date, "2026-08-01");
    assert_eq!(a[0].uninstall.prog, r"C:\Users\X\AppData\Local\Programs\SideQuest\Uninstall SideQuest.exe");
    assert_eq!(a[0].uninstall.args, vec!["/currentuser"]);
    assert_eq!(a[1].uninstall, Cmd::new("MsiExec.exe", &["/X{1234-5678}"]));
}

#[test]
fn storage_parse_pacman() {
    let qi = "Name            : firefox\nVersion         : 130.0-1\nDescription     : Standalone web browser\nInstalled Size  : 245.67 MiB\nInstall Date    : Mon 01 Sep 2026 10:11:12 AM CEST\n\nName            : zsh\nVersion         : 5.9-5\nDescription     : A very advanced shell\nInstalled Size  : 7.12 MiB\nInstall Date    : Tue 02 Sep 2026 09:00:00 AM CEST\n";
    let a = sys::parse_pacman_qi(qi);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].name, "firefox");
    assert_eq!(a[0].size, (245.67 * 1024.0 * 1024.0) as u64);
    assert_eq!(a[0].date, "01 Sep 2026");
    assert_eq!(a[1].uninstall, Cmd::new("sudo", &["pacman", "-Rns", "zsh"]));
    let ss = "extra/ripgrep 14.1.1-1 [installed]\n    A search tool\naur/ripgrep-git 14.1.0.r12-1 (+3 0.00)\n    ripgrep from git\n";
    let f = sys::parse_pacman_ss(ss, Some("yay"));
    assert_eq!(f.len(), 2);
    assert_eq!(f[0].source, "extra ✓");
    assert_eq!(f[1].desc, "ripgrep from git");
    assert_eq!(f[1].install, Cmd::new("yay", &["-S", "ripgrep-git"]));
    let f = sys::parse_pacman_ss(ss, None);
    assert_eq!(f[0].install, Cmd::new("sudo", &["pacman", "-S", "ripgrep"]));
}

#[test]
fn storage_parse_sizes() {
    assert_eq!(scan::parse_journal("Archived and active journals take up 1.5G in the file system."), Some(1610612736));
    assert_eq!(scan::parse_size("512.0M"), Some(512 << 20));
    assert_eq!(scan::parse_size("12 KiB"), Some(12 << 10));
    assert_eq!(super::human(15 << 30), "15.0 GB");
    assert_eq!(super::human(900), "900 B");
}
