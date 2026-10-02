//! QA key-mash for the storage app (see src/qa_keys.rs). Under cfg(test) the pane only records cleans, installs,
//! uninstalls and web pages; on top of that this keeps it off the real disk and the network: the cleanup list,
//! installed apps, catalog marks and search results are fakes, "big folders" is rooted in a scratch tree it may
//! not climb out of, and every key that would rescan the machine or search winget is held back.

use super::scan::{self, Cmd, Find, How, Kind, Target};
use super::*;
use crate::qa_keys::{Ev, Opts, mash_pane, scratch};
use crate::testkit::Kit;
use crossterm::event::{KeyCode, KeyModifiers};

fn target(id: &'static str, label: &str, kind: Kind, how: How, path: &str) -> Target {
    Target { id, label: label.into(), paths: vec![PathBuf::from(path)], kind, note: format!("why {label} 中文"), how, find: Find::Fixed, open: Some(PathBuf::from(path)) }
}

fn tree(root: &Path) {
    for d in ["big/inner", "small", "deep/a/b/c", "ünï 文件", "empty"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join("big/blob.bin"), vec![7u8; 300_000]).unwrap();
    std::fs::write(root.join("big/inner/more.bin"), vec![1u8; 120_000]).unwrap();
    std::fs::write(root.join("small/a.txt"), "hi").unwrap();
    std::fs::write(root.join("deep/a/b/c/leaf.txt"), "leaf").unwrap();
    std::fs::write(root.join("ünï 文件/名前.txt"), "x".repeat(5000)).unwrap();
    std::fs::write(root.join("loose.txt"), "loose").unwrap();
}

/// A Storage wired to the kit, with fake data everywhere a real scan or query would go.
fn safe(k: &Kit, root: &Path) -> Storage {
    let mut p = Storage::new();
    p.started = true; // no cleanup scan, no drive scan
    p.out.waker = Some(crate::pane::Waker { id: 1, tx: k.tx.clone() });
    p.drives = Some(vec![
        scan::Drive { name: "C:".into(), total: 952 << 30, free: 28 << 30 },
        scan::Drive { name: "D:".into(), total: 2000 << 30, free: 2000 << 30 },
        scan::Drive { name: "Z:".into(), total: 0, free: 0 },
    ]);
    p.targets = vec![
        target("temp", "temp files", Kind::Clean, How::Dir, "/nowhere/temp"),
        target("npm", "npm cache", Kind::Clean, How::Dir, "/nowhere/npm"),
        target("pacman", "pacman package cache", Kind::Clean, How::Term(Cmd::new("sudo", &["paccache", "-rk1"])), "/nowhere/pacman"),
        target("pnpm", "pnpm store", Kind::Clean, How::Pnpm, "/nowhere/pnpm"),
        target("bin", "recycle bin", Kind::Clean, How::Recycle, "/nowhere/bin"),
        target("downloads", "Downloads 🙂", Kind::Review, How::Nothing, "/nowhere/Downloads"),
    ];
    for (id, n, done) in [("temp", 15u64 << 30, true), ("npm", 700 << 20, false), ("pacman", 0, true), ("pnpm", u64::MAX / 2, true), ("downloads", 30 << 30, true)] {
        p.sizes.insert(id, (n, done));
    }
    p.froot = root.to_path_buf(); // big folders measures this scratch tree, nothing else
    let cmd = |s: &str| Cmd::new("echo", &[s]);
    p.apps = Some(Ok((0..25)
        .map(|i| App {
            name: format!("App {i} {}", ["", "中文", "🙂", "a very long application name indeed, much longer than the column"][i % 4]),
            version: format!("{i}.0"),
            publisher: if i % 3 == 0 { String::new() } else { "Pub".into() },
            size: (i as u64) << 24,
            date: if i % 2 == 0 { "2026-01-01".into() } else { String::new() },
            uninstall: cmd("uninstall"),
        })
        .collect()));
    let mut inst = catalog::Installed::default();
    inst.ids.insert("winget:git.git".into());
    inst.names.push("discord".into());
    p.installed = Some(inst);
    p.found_q = "rip".into();
    p.found = Some(Ok((0..12)
        .map(|i| Found { name: format!("Found {i} ✦"), id: format!("Pkg.{i}"), version: "1.0".into(), source: "winget".into(), desc: "d".repeat(i * 9), install: cmd("install") })
        .collect()));
    p
}

/// Holds back what would scan the real disk, list the real registry / winget, or search the network.
fn guard(p: &mut Storage, root: &Path, ev: &mut Ev) -> bool {
    let Ev::Key(k) = ev else { return true };
    if p.confirm.is_some() || k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) {
        return true; // a question takes every key (and its yes only records, under cfg(test))
    }
    if p.input {
        // enter in the catalog filter (no match) or the install box runs a winget search
        return !(k.code == KeyCode::Enter && matches!(p.view, View::Catalog | View::Install));
    }
    match k.code {
        KeyCode::Char('s') if p.view == View::Catalog => false,     // winget search for the filter text
        KeyCode::Char('r') => p.view == View::Folders,             // rescans: only the scratch tree is ok
        KeyCode::Backspace if p.view == View::Folders => p.froot != root, // up, out of the scratch tree
        _ => true,
    }
}

#[test]
fn qa_keys_storage() {
    let root = scratch("storage");
    tree(&root);
    let r2 = root.clone();
    mash_pane("storage", Opts { snap: Some("storage".into()), ..Default::default() }, move |k| safe(k, &root), move |p, ev| {
        let ok = guard(p, &r2, ev);
        assert!(p.froot.starts_with(&r2), "big folders left the scratch tree: {}", p.froot.display());
        ok
    });
}

/// Same, on an empty machine: no drives, no cleanup rows, no apps, empty search results and errors.
#[test]
fn qa_keys_storage_empty() {
    let root = scratch("storage-empty");
    let r2 = root.clone();
    mash_pane(
        "storage-empty",
        Opts { keys: crate::qa_keys::keys_for(1500), ..Default::default() },
        move |k| {
            let mut p = safe(k, &root);
            p.drives = Some(vec![]);
            p.targets.clear();
            p.sizes.clear();
            p.apps = Some(Err("couldn't list apps 中文".into()));
            p.found = Some(Ok(vec![]));
            p.installed = Some(catalog::Installed::default());
            p
        },
        move |p, ev| guard(p, &r2, ev),
    );
}

/// No pane here opens anything for real: everything it launched was only recorded.
#[test]
fn qa_keys_storage_records_only() {
    let root = scratch("storage-records");
    tree(&root);
    let mut k = Kit::new();
    let mut p = safe(&k, &root);
    for c in ['x', 'y', '3', 'u', 'y', '4', 'i', 'y', 'o', '5', 'j', '\n', 'y'] {
        let code = if c == '\n' { KeyCode::Enter } else { KeyCode::Char(c) };
        k.key(&mut p, code);
        k.render(&mut p, 120, 40);
    }
    assert!(!p.launched.is_empty(), "the yeses were recorded: {:?}", p.launched);
}
