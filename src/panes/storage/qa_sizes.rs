//! QA sizes for storage (see crate::qa_sizes). Storage measures the real home folder the moment it's drawn,
//! so this sweeps a fixture-backed pane instead: every view filled with awkward names, and the confirm
//! dialog, with nothing started (no scan, no winget, no disk access).

use super::scan::{Cmd, Drive, Find, How, Kind, Target};
use super::*;
use crate::qa_sizes::{AWKWARD, Known, outside, sweep, verdict};
use crate::testkit::Kit;

const IDS: [&str; 6] = ["q-temp", "q-npm", "q-root", "q-downloads", "q-cache", "q-x"];

/// A storage pane in `view` with data in every view and nothing running.
fn fixture(tx: &std::sync::mpsc::Sender<crate::pane::Event>, view: View, confirm: bool) -> Storage {
    let env = catalog::Env { os: catalog::Os::Windows, aur_helper: None, flatpak: false, npm: true };
    let mut p = Storage::with_env(env, "winget");
    p.started = true;
    p.out.waker = Some(crate::pane::Waker { id: 1, tx: tx.clone() });
    p.drives = Some(vec![
        Drive { name: "C:".into(), total: 952 << 30, free: 28 << 30 },
        Drive { name: AWKWARD[1].into(), total: 2000 << 30, free: 1 << 20 },
        Drive { name: AWKWARD[0].into(), total: 0, free: 0 },
    ]);
    p.targets = IDS
        .iter()
        .enumerate()
        .map(|(i, &id)| Target {
            id,
            label: AWKWARD[i % AWKWARD.len()].into(),
            paths: vec![PathBuf::from(format!("/nowhere/{}", AWKWARD[i % AWKWARD.len()]))],
            kind: if i % 3 == 2 { Kind::Review } else { Kind::Clean },
            note: AWKWARD[(i + 1) % AWKWARD.len()].into(),
            how: if i == 2 { How::Term(Cmd::new("sudo", &["paccache", "-rk1"])) } else if i % 3 == 2 { How::Nothing } else { How::Dir },
            find: Find::Fixed,
            open: Some(PathBuf::from("/nowhere")),
        })
        .collect();
    for (i, id) in IDS.iter().enumerate() {
        p.sizes.insert(id, ((i as u64 + 1) << (20 + i * 3), i % 2 == 0));
    }
    p.froot = PathBuf::from(format!("/nowhere/{}/{}", AWKWARD[0], AWKWARD[1]));
    p.fstarted = true;
    p.fdone = true;
    p.frows = (0..40)
        .map(|i| FRow { name: format!("{} {i}", AWKWARD[i % AWKWARD.len()]), path: PathBuf::from(format!("/nowhere/{i}")), dir: i % 2 == 0, size: (i as u64 * 7919) << 20, done: i % 5 != 0 })
        .collect();
    p.fbig = AWKWARD.iter().map(|a| (5 << 30, PathBuf::from(format!("/nowhere/{a}")))).collect();
    p.apps = Some(Ok((0..30)
        .map(|i| sys::App { name: AWKWARD[i % AWKWARD.len()].into(), version: "1.2.3-beta.4+build.5678".into(), publisher: AWKWARD[(i + 1) % AWKWARD.len()].into(), size: (i as u64) << 24, date: "2026-09-26".into(), uninstall: Cmd::new("x", &[]) })
        .collect()));
    let mut inst = catalog::Installed::default();
    inst.ids.insert("winget:git.git".into());
    inst.names.push("discord".into());
    p.installed = Some(inst);
    p.found_q = AWKWARD[0].into();
    let text = format!(
        "Name             Id                      Version Match        Source\n\
         --------------------------------------------------------------------\n\
         {}      Some.Id.{}  15.2.0  Tag: x winget\n\
         RipGrep MSVC     BurntSushi.ripgrep.MSVC 15.2.0  Tag: ripgrep winget\n",
        AWKWARD[4], AWKWARD[4]
    );
    p.found = Some(Ok(sys::parse_winget(&text)));
    p.view = view;
    if confirm {
        p.confirm = Some(Confirm { question: format!("{}?", AWKWARD[0]), lines: AWKWARD.iter().map(|s| s.to_string()).collect(), what: Pending::Clean("q-temp") });
    }
    p
}

/// For the whole-app sweep: the storage app tab without touching the disk.
pub(crate) fn app_pane(tx: &std::sync::mpsc::Sender<crate::pane::Event>) -> Box<dyn crate::pane::Pane> {
    Box::new(fixture(tx, View::Cleanup, false))
}

/// Bugs found by this sweep (each has its own ignored test below).
const KNOWN: &[Known] = &[];

#[test]
fn qa_sizes_storage() {
    let mut k = Kit::new();
    let mut bad = vec![];
    for view in [View::Cleanup, View::Folders, View::Apps, View::Catalog, View::Install] {
        for confirm in [false, true] {
            let name = format!("storage({view:?}{})", if confirm { "+confirm" } else { "" });
            bad.extend(sweep(&name, &mut k, &mut |k| Box::new(fixture(&k.tx, view, confirm)), !confirm, 0));
        }
    }
    let _ = outside;
    verdict(bad, KNOWN);
}
