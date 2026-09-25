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
    k.key(&mut p, KeyCode::Char('5'));
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

// ------------------------------------------------------------------ get apps (catalog)

use super::catalog::{self, CATALOG, Env, Os, Src};

fn env(os: Os, aur: Option<&'static str>, flatpak: bool, npm: bool) -> Env {
    Env { os, aur_helper: aur, flatpak, npm }
}

fn idx(name: &str) -> usize {
    CATALOG.iter().position(|e| e.name == name).unwrap_or_else(|| panic!("{name} not in the catalog"))
}

/// A catalog pane on a pretend OS with a pretend "installed" list, so nothing real is queried.
fn catalog_pane(k: &Kit, e: Env, installed: &[&str]) -> Storage {
    let via = match e.os { Os::Windows => "winget", Os::Arch => "pacman + AUR", _ => "pacman" };
    let mut p = Storage::with_env(e, via);
    quiet(&mut p, k);
    fake(&mut p);
    let mut inst = catalog::Installed::default();
    for i in installed {
        inst.ids.insert(i.to_lowercase());
    }
    inst.names.push("discord".into()); // an ARP-only row, matched by name
    p.installed = Some(inst);
    p
}

#[test]
fn storage_catalog_snapshot() {
    let mut k = Kit::new();
    let mut p = catalog_pane(&k, env(Os::Windows, None, false, true), &["winget:Valve.Steam", "winget:Git.Git", "npm:@anthropic-ai/claude-code", "winget:OBSProject.OBSStudio"]);
    k.key(&mut p, KeyCode::Char('4'));
    assert_eq!(p.view, View::Catalog);
    assert!(!p.input, "the catalog opens on the list, not the box");
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-catalog.html");
    println!("{out}\n{:?}", p.subtitle());
    assert!(out.contains("categories") && out.contains("creative") && out.contains("utilities"));
    assert!(out.contains("OBS Studio") && out.contains("✓ installed"));
    // Linux-only apps are hidden on Windows
    assert!(!out.contains("btop") && !out.contains("Lutris"));
    let side = k.render_side(&mut p, 34, 8);
    println!("{side}");
    assert!(side.contains("get apps"));
    // category 2 = gaming
    k.key(&mut p, KeyCode::Char('3'));
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-catalog-gaming.html");
    println!("{out}");
    assert!(out.contains("Steam") && out.contains("Playnite") && !out.contains("OBS Studio"));
    let d = idx("Discord");
    assert!(p.installed.as_ref().unwrap().has(&CATALOG[d]), "ARP name match");
}

#[test]
fn storage_catalog_arch_snapshot() {
    let mut k = Kit::new();
    let mut p = catalog_pane(&k, env(Os::Arch, Some("yay"), true, false), &["pkg:firefox", "pkg:btop", "flatpak:com.spotify.Client"]);
    k.key(&mut p, KeyCode::Char('4'));
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-catalog-arch.html");
    println!("{out}\n{:?}", p.subtitle());
    assert!(out.contains("pacman") && out.contains("aur"));
    assert!(!out.contains("PowerToys"), "Windows-only apps are hidden on Arch");
    // npm CLIs show up muted with the reason when npm is missing
    k.key(&mut p, KeyCode::Char('5')); // ai
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-catalog-arch-ai.html");
    println!("{out}");
    assert!(out.contains("Claude Code") && out.contains("needs npm"));
    let cc = idx("Claude Code");
    p.sel[View::Catalog.i()] = cc;
    k.key(&mut p, KeyCode::Enter);
    assert!(p.confirm.is_none(), "nothing to confirm without npm");
    assert!(k.notices().iter().any(|n| n.contains("needs npm")));
    assert!(p.launched.is_empty());
}

#[test]
fn storage_catalog_install_needs_confirmation() {
    let mut k = Kit::new();
    let mut p = catalog_pane(&k, env(Os::Windows, None, false, true), &[]);
    k.key(&mut p, KeyCode::Char('4'));
    // first row of "all" is OBS Studio
    assert_eq!(p.selected(), Some(idx("OBS Studio")));
    k.key(&mut p, KeyCode::Char('j'));
    assert_eq!(p.selected(), Some(idx("Blender")));
    k.key(&mut p, KeyCode::Enter);
    let c = p.confirm.as_ref().expect("asks first");
    assert_eq!(c.question, "install Blender?");
    assert!(c.lines.iter().any(|l| l.contains("winget install --id BlenderFoundation.Blender -e --accept-package-agreements")));
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-catalog-confirm.html");
    println!("{out}");
    k.key(&mut p, KeyCode::Char('j')); // ignored while asking
    k.key(&mut p, KeyCode::Esc);
    assert!(p.confirm.is_none() && p.launched.is_empty());
    k.key(&mut p, KeyCode::Enter);
    k.key(&mut p, KeyCode::Char('y'));
    assert_eq!(p.launched, vec!["term: winget install --id BlenderFoundation.Blender -e --accept-package-agreements --accept-source-agreements".to_string()]);
    // o opens the homepage (recorded only)
    k.key(&mut p, KeyCode::Char('o'));
    assert_eq!(p.launched.last().map(String::as_str), Some("open: https://www.blender.org"));
    // DaVinci Resolve has no package: enter opens its download page, nothing to install
    p.sel[View::Catalog.i()] = idx("DaVinci Resolve");
    k.key(&mut p, KeyCode::Enter);
    assert!(p.confirm.is_none());
    assert!(p.launched.last().unwrap().starts_with("open: https://www.blackmagicdesign.com"));
}

#[test]
fn storage_catalog_mouse_and_categories() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut k = Kit::new();
    let mut p = catalog_pane(&k, env(Os::Windows, None, false, true), &[]);
    k.key(&mut p, KeyCode::Char('4'));
    k.render(&mut p, 150, 44);
    let click = |x: u16, y: u16| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: crossterm::event::KeyModifiers::NONE };
    let area = ratatui::layout::Rect::new(0, 0, 150, 44);
    // click the "ai" category row
    let (r, _) = *p.cat_hits.iter().find(|(_, c)| *c == 4).unwrap();
    k.mouse(&mut p, click(r.x + 3, r.y), area);
    assert_eq!(p.cat, 4);
    k.render(&mut p, 150, 44);
    // single click on the second table row selects, a second click on it asks to install
    let (body, off) = p.table_hit.unwrap();
    let second = p.order(View::Catalog)[off + 1];
    k.mouse(&mut p, click(body.x + 2, body.y + 1), area);
    assert_eq!(p.selected(), Some(second));
    assert!(p.confirm.is_none());
    k.mouse(&mut p, click(body.x + 2, body.y + 1), area);
    assert!(p.confirm.is_some(), "double-click asks");
    k.key(&mut p, KeyCode::Char('n'));
    assert!(p.launched.is_empty());
    // tab walks categories, then leaves for the search view; backtab comes back to the last category
    p.set_cat(catalog::CATS.len());
    k.key(&mut p, KeyCode::Tab);
    assert_eq!(p.view, View::Install);
    p.input = false;
    k.key(&mut p, KeyCode::BackTab);
    assert_eq!((p.view, p.cat), (View::Catalog, catalog::CATS.len()));
    // the filter searches every category
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "vpn");
    let names: Vec<&str> = p.order(View::Catalog).iter().map(|&i| CATALOG[i].name).collect();
    println!("{names:?}");
    assert!(names.contains(&"Mullvad VPN") && names.contains(&"Proton VPN"));
    k.key(&mut p, KeyCode::Esc);
    assert!(p.cfilter.is_empty());
}

#[test]
fn storage_catalog_picks_per_os() {
    let pk = |name: &str, e: &Env| catalog::pick(&CATALOG[idx(name)], e);
    let arch = env(Os::Arch, Some("paru"), true, true);
    let arch_bare = env(Os::Arch, None, false, false);
    let deb = env(Os::Debian, None, true, false);
    let win = env(Os::Windows, None, false, false);
    // pacman first, AUR through the helper
    assert_eq!(pk("Firefox", &arch).unwrap().install, Some(Cmd::new("sudo", &["pacman", "-S", "--needed", "firefox"])));
    assert_eq!(pk("Brave", &arch).unwrap().install, Some(Cmd::new("paru", &["-S", "brave-bin"])));
    // no helper: flatpak, and with no flatpak either: say what's missing
    assert_eq!(pk("Brave", &env(Os::Arch, None, true, false)).unwrap().src, Src::Flatpak);
    let b = pk("Brave", &arch_bare).unwrap();
    assert!(b.install.is_none() && b.missing.as_deref().unwrap().contains("yay or paru"));
    // several packages in one field
    assert_eq!(pk("Node.js LTS", &deb).unwrap().install, Some(Cmd::new("sudo", &["apt", "install", "nodejs", "npm"])));
    assert_eq!(pk("Spotify", &deb).unwrap().install, Some(Cmd::new("flatpak", &["install", "-y", "flathub", "com.spotify.Client"])));
    // hidden where there's nothing
    assert!(pk("PowerToys", &arch).is_none());
    assert!(pk("btop", &win).is_none());
    // website fallback
    assert_eq!(pk("Tailscale", &deb).unwrap().src, Src::Web);
    assert_eq!(pk("DaVinci Resolve", &win).unwrap().src, Src::Web);
    // npm
    let npm = env(Os::Linux, None, false, true);
    assert!(pk("Codex", &npm).unwrap().install.unwrap().text().ends_with("npm i -g @openai/codex")); // cmd /c on Windows
    assert!(pk("Codex", &win).unwrap().missing.is_some());
    // every entry has a name, description, homepage, and at least one way in
    for e in CATALOG {
        assert!(!e.name.is_empty() && !e.desc.is_empty() && e.home.starts_with("https://"), "{}", e.name);
        assert!(e.web || [e.winget, e.pacman, e.aur, e.apt, e.flatpak, e.npm].iter().any(|s| !s.is_empty()), "{}", e.name);
    }
}

#[test]
fn storage_catalog_parsers() {
    assert_eq!(catalog::distro("NAME=\"Omarchy\"\nID=omarchy\nID_LIKE=arch\n"), Os::Arch);
    assert_eq!(catalog::distro("ID=arch\n"), Os::Arch);
    assert_eq!(catalog::distro("ID=pop\nID_LIKE=\"ubuntu debian\"\n"), Os::Debian);
    assert_eq!(catalog::distro("ID=fedora\n"), Os::Linux);
    let list = "   - \r   \\ \r\nName                 Id                                      Version     Available  Source\n\
                -----------------------------------------------------------------------------------------------\n\
                Discord              ARP\\User\\X64\\Discord                     1.0.9258\n\
                Git                  Git.Git                                 2.55.0.3               winget\n\
                Steam                Valve.Steam                             2.10.91.91             winget\n";
    let (ids, names) = catalog::parse_winget_list(list);
    assert_eq!(ids, vec!["Git.Git", "Valve.Steam"]);
    assert_eq!(names, vec!["discord"]);
    let npm = "C:\\Users\\x\\AppData\\Roaming\\npm\\node_modules\nC:\\Users\\x\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\nC:\\Users\\x\\AppData\\Roaming\\npm\\node_modules\\pnpm\n";
    assert_eq!(catalog::parse_npm_ls(npm), vec!["@anthropic-ai/claude-code", "pnpm"]);
    // whole-word name matching: "git" is not "github desktop"
    let mut inst = catalog::Installed::default();
    inst.names = vec!["github desktop".into()];
    assert!(!inst.has(&CATALOG[idx("Git")]));
    inst.names = vec!["python 3.13.1 (64-bit)".into()];
    assert!(inst.has(&CATALOG[idx("Python")]));
}

/// Reads this machine's real installed list (winget list / pacman -Qq …, read-only) and snapshots the
/// catalog with it: `cargo test storage_catalog_live -- --ignored --nocapture`.
#[test]
#[ignore]
fn storage_catalog_live() {
    let mut k = Kit::new();
    let mut p = Storage::new();
    quiet(&mut p, &k);
    p.refresh_drives();
    k.key(&mut p, KeyCode::Char('4'));
    for _ in 0..300 {
        k.wait_wake(&mut p, 100);
        if p.installed.is_some() && p.drives.is_some() {
            break;
        }
    }
    let out = k.render_html(&mut p, 150, 44, "target/snap/storage-catalog-live.html");
    println!("{out}\n{:?}", p.subtitle());
    assert!(p.installed.is_some());
}