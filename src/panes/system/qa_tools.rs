//! QA: the system app with fake data only (no sampler or probe threads, a fake killer): odd and extreme
//! snapshots in every view at every size, parent-pid cycles in the tree, and filters.

use super::probes::{Conn, Info, Section, Service, StartupItem};
use super::sampler::{DiskRow, Gpu, GpuState, Proc, Snap};
use super::*;
use crate::testkit::Kit;
use std::collections::VecDeque;

fn never_kill(_pid: u32) -> Result<(), String> {
    Ok(())
}

fn quiet() -> System {
    let mut p = System::new();
    p.started = true; // no sampler thread
    p.probes_live = false; // no probe threads
    p.killer = never_kill;
    p
}

fn proc(pid: u32, ppid: u32, name: &str, start: u64) -> Proc {
    Proc { pid, name: name.into(), cpu: 1.0, mem: 1 << 20, threads: 1, user: Arc::from(""), ppid, start, read: 0.0, write: 0.0 }
}

fn set(p: &mut System, snap: Snap) {
    p.snap = Arc::new(snap);
    p.rebuild();
}

/// Extreme and broken values everywhere: nothing may panic or divide by zero.
fn weird_snap() -> Snap {
    let mut procs = vec![
        proc(0, 0, "", 0),                                                  // pid 0, no name
        proc(10, 20, "cycle-a", 0),                                         // a ↔ b parent cycle
        proc(20, 10, "cycle-b", 0),
        proc(30, 30, "own-parent", 0),                                      // its own parent
        proc(40, 10, "child of a", 5),
        proc(50, 999_999, "orphan", 5),                                     // parent not in the list
        proc(60, 70, "older than its parent (pid reuse)", 0),
        proc(70, 0, "young parent", 100),
        proc(80, 0, &"very-long-process-name-".repeat(20), 1),
        proc(90, 0, "日本語のプロセス 🎉 ünïcödé", 1),
        proc(100, 0, "control\u{1b}[31mchars\n\ttab", 1),
    ];
    procs[1].cpu = f32::NAN;
    procs[2].cpu = f32::INFINITY;
    procs[3].cpu = -5.0;
    procs[4].mem = u64::MAX;
    procs[5].read = f64::NAN;
    procs[6].write = f64::INFINITY;
    procs[7].threads = u32::MAX;
    Snap {
        seq: 7,
        cpu_hist: [f32::NAN, f32::INFINITY, -1.0, 250.0, 0.0].into_iter().collect(),
        cores: vec![f32::NAN, 150.0, -3.0],
        core_hist: vec![VecDeque::from([f32::NAN, 1e9])], // shorter than `cores`
        core_mhz: vec![],
        ghz: f64::NAN,
        ram_used: 5_000_000_000,
        ram_total: 0, // divide by zero?
        ram_hist: [f32::NAN, 1e9].into_iter().collect(),
        disks: vec![DiskRow { label: "".into(), total: 0, free: 10 }, DiskRow { label: "Z: ".repeat(40), total: 10, free: 100 }],
        net_down: [f64::NAN, f64::INFINITY, -1.0].into_iter().collect(),
        net_up: VecDeque::new(),
        disk_r: [1e300].into_iter().collect(),
        disk_w: [0.0].into_iter().collect(),
        threads: u64::MAX,
        procs,
        sample_ms: f64::NAN,
    }
}

fn many_cores() -> Snap {
    let n = 256;
    Snap {
        seq: 3,
        cores: (0..n).map(|i| (i % 100) as f32).collect(),
        core_hist: (0..n).map(|_| (0..120).map(|i| (i % 100) as f32).collect()).collect(),
        core_mhz: vec![4200; n],
        ram_used: 1,
        ram_total: 2,
        procs: (0..5000).map(|i| proc(i + 1, if i > 0 { (i + 1) / 2 } else { 0 }, &format!("p{i}"), i as u64)).collect(),
        ..Default::default()
    }
}

fn odd_lists(p: &mut System) {
    p.startup = Some(Arc::new(vec![StartupItem { name: "".into(), command: "".into(), location: "".into(), enabled: None }, StartupItem { name: "ünï 🎉".into(), command: "x ".repeat(300), location: "HKCU Run".into(), enabled: Some(false) }]));
    p.services = Some(Arc::new(vec![Service { name: "".into(), display: "".into(), state: "".into(), start: "".into(), pid: 0, desc: "".into() }, Service { name: "s".repeat(200), display: "日本".into(), state: "weird-state".into(), start: "?".into(), pid: u32::MAX, desc: "d ".repeat(500) }]));
    p.conns = Some(Arc::new(vec![Conn { proto: "".into(), local: "".into(), remote: "".into(), state: "".into(), pid: 0, process: "".into() }, Conn { proto: "tcp6".into(), local: "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535".into(), remote: "[::]:0".into(), state: "established".into(), pid: 4, process: "System".into() }]));
    p.info = Some(Arc::new(Info { boot: u64::MAX, sections: vec![Section { icon: "window", title: "".into(), rows: vec![] }, Section { icon: "nope-not-an-icon", title: "t".repeat(300), rows: vec![("".into(), "".into()), ("k".repeat(200), "v".repeat(500))] }] }));
    for i in 0..3 {
        p.rebuild_list(i);
    }
}

#[test]
fn qa_system_odd_data_every_view_every_size() {
    let mut k = Kit::new();
    let sizes: &[(u16, u16)] = &[(1, 1), (2, 2), (5, 3), (10, 4), (19, 6), (20, 6), (39, 10), (40, 12), (60, 20), (79, 24), (100, 30), (150, 44), (300, 90)];
    let mut failures = vec![];
    for (label, snap, gpu) in [
        ("empty", Snap::default(), GpuState::Pending),
        ("weird", weird_snap(), GpuState::Ok(Gpu { name: "".into(), util: f32::NAN, used: 1.0, total: 0.0, temp: f32::INFINITY, power: -1.0, limit: 0.0, fan: 500.0 })),
        ("256 cores, 5000 procs", many_cores(), GpuState::Failed),
    ] {
        let mut p = quiet();
        set(&mut p, snap);
        p.gpu = gpu;
        if label != "empty" {
            odd_lists(&mut p);
        }
        for key in ['1', '2', '3', '4', '5', '6', '7'] {
            for tree in [false, true] {
                if tree && key != '2' {
                    continue;
                }
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    k.key(&mut p, KeyCode::Char(key));
                    if tree {
                        k.key(&mut p, KeyCode::Char('t'));
                    }
                    for &(w, h) in sizes {
                        k.render(&mut p, w, h);
                        k.render_side(&mut p, w.min(34), h);
                        for code in [KeyCode::Char('G'), KeyCode::PageDown, KeyCode::Char('g'), KeyCode::Down, KeyCode::Left, KeyCode::Right] {
                            k.key(&mut p, code);
                        }
                        k.render(&mut p, w, h);
                    }
                    for s in ['c', 'm', 'r', 'w', 'n'] {
                        if key == '1' || key == '2' {
                            k.key(&mut p, KeyCode::Char(s));
                        }
                    }
                    k.render(&mut p, 150, 44);
                    if tree {
                        k.key(&mut p, KeyCode::Char('t'));
                    }
                }));
                if let Err(e) = r {
                    let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    failures.push(format!("{label}, view {key}{}: {msg}", if tree { " (tree)" } else { "" }));
                }
            }
        }
    }
    assert!(failures.is_empty(), "panics:\n{}", failures.join("\n"));
}

#[test]
fn qa_system_tree_survives_parent_cycles() {
    let mut k = Kit::new();
    let mut p = quiet();
    set(&mut p, weird_snap());
    k.key(&mut p, KeyCode::Char('2'));
    let t = std::time::Instant::now();
    k.key(&mut p, KeyCode::Char('t'));
    assert!(t.elapsed().as_secs() < 5, "tree took {:?}", t.elapsed());
    let n = p.snap.procs.len();
    assert_eq!(p.rows.len(), n, "every process shown exactly once");
    let mut seen: Vec<usize> = p.rows.clone();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), n);
    // a child that started before its "parent" (a reused pid) is not put under it
    let pos = |name: &str| p.rows.iter().position(|&i| p.snap.procs[i].name == name).unwrap();
    let old = pos("older than its parent (pid reuse)");
    assert_eq!(p.tree_rows[old].depth, 0, "pid reuse: shown as its own root");
    assert!(p.tree_rows[pos("child of a")].depth >= 1);
    // fold everything that has children, then unfold: same rows back
    for i in 0..p.rows.len() {
        p.sel = i.min(p.rows.len().saturating_sub(1));
        k.key(&mut p, KeyCode::Left);
    }
    for _ in 0..3 {
        for i in 0..p.rows.len() {
            p.sel = i;
            k.key(&mut p, KeyCode::Right);
        }
    }
    let out = k.render(&mut p, 150, 44);
    assert!(out.contains("cycle-a") && out.contains("cycle-b"), "{out}");
    // filters keep matches (and their ancestors), even through a cycle
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "cycle");
    let names: Vec<&str> = p.rows.iter().map(|&i| p.snap.procs[i].name.as_str()).collect();
    assert!(names.contains(&"cycle-a") && names.contains(&"cycle-b"), "{names:?}");
    k.key(&mut p, KeyCode::Esc);
}

/// The pids the recording killer was asked to end (only `qa_system_kill_hits_the_pid_it_asked_about` uses it).
static KILLED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

fn record_kill(pid: u32) -> Result<(), String> {
    KILLED.lock().unwrap().push(pid);
    Ok(())
}

/// x asks about the highlighted process; y must end THAT pid even if a new snapshot re-sorted the table in
/// between (the sampler replaces it every second or so), and n / esc end nothing.
#[test]
fn qa_system_kill_hits_the_pid_it_asked_about() {
    let mut k = Kit::new();
    let mut p = quiet();
    p.killer = record_kill;
    let mk = |cpus: [f32; 3]| {
        let mut procs = vec![proc(111, 0, "alpha.exe", 1), proc(222, 0, "beta.exe", 1), proc(333, 0, "gamma.exe", 1)];
        for (pr, c) in procs.iter_mut().zip(cpus) {
            pr.cpu = c;
        }
        procs
    };
    set(&mut p, Snap { seq: 1, procs: mk([50.0, 10.0, 5.0]), ram_total: 1, ..Default::default() });
    k.key(&mut p, KeyCode::Char('2'));
    k.key(&mut p, KeyCode::Char('g'));
    let asked = p.selected().map(|x| (x.pid, x.name.clone())).expect("a process is highlighted");
    k.key(&mut p, KeyCode::Char('x'));
    assert_eq!(p.pending_kill.as_ref().map(|(pid, _)| *pid), Some(asked.0));
    let s = k.render(&mut p, 150, 40);
    assert!(s.contains(&asked.1) && s.contains("y") && s.contains("kill it"), "the question names it:\n{s}");
    // a fresh snapshot arrives while the question is up: another process is now on top
    set(&mut p, Snap { seq: 2, procs: mk([1.0, 90.0, 5.0]), ram_total: 1, ..Default::default() });
    let now_top = p.selected().map(|x| x.pid);
    k.key(&mut p, KeyCode::Char('y'));
    k.wait_wake(&mut p, 500);
    let killed = std::mem::take(&mut *KILLED.lock().unwrap());
    assert_eq!(killed, [asked.0], "asked about {asked:?}, highlighted is now {now_top:?}");
    assert!(k.notices().iter().any(|n| n.contains(&format!("pid {}", asked.0))), "{:?}", k.notices());
    // n and esc leave it alone
    for no in [KeyCode::Char('n'), KeyCode::Esc, KeyCode::Enter] {
        k.key(&mut p, KeyCode::Char('x'));
        assert!(p.pending_kill.is_some());
        k.key(&mut p, no);
        assert!(p.pending_kill.is_none());
    }
    k.wait_wake(&mut p, 200);
    assert!(KILLED.lock().unwrap().is_empty(), "n / esc / enter killed {:?}", KILLED.lock().unwrap());
    assert!(k.notices().iter().filter(|n| n.as_str() == "left it alone").count() == 3, "{:?}", k.notices());
}

#[test]
fn qa_system_filters_and_kill_with_nothing_selected() {
    let mut k = Kit::new();
    let mut p = quiet();
    set(&mut p, weird_snap());
    // a pid matches exactly, not as a substring
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "20");
    let names: Vec<&str> = p.rows.iter().map(|&i| p.snap.procs[i].name.as_str()).collect();
    assert_eq!(names, ["cycle-b"], "pid 20 only");
    for _ in 0..2 {
        k.key(&mut p, KeyCode::Backspace);
    }
    k.typ(&mut p, "ÜNÏCÖDÉ");
    assert_eq!(p.rows.len(), 1, "unicode filter ignores case");
    for _ in 0..20 {
        k.key(&mut p, KeyCode::Backspace);
    }
    k.typ(&mut p, "no such process at all");
    assert!(p.rows.is_empty());
    let out = k.render(&mut p, 150, 44);
    k.key(&mut p, KeyCode::Enter);
    // x with an empty table asks nothing
    k.key(&mut p, KeyCode::Char('x'));
    assert!(p.pending_kill.is_none(), "{out}");
    k.key(&mut p, KeyCode::Char('y'));
    k.key(&mut p, KeyCode::Esc);
    // an empty snapshot too, in every view with a list
    let mut p = quiet();
    for key in ['1', '2', '4', '5', '6'] {
        k.key(&mut p, KeyCode::Char(key));
        k.key(&mut p, KeyCode::Char('x'));
        assert!(p.pending_kill.is_none(), "view {key}");
        k.key(&mut p, KeyCode::Char('/'));
        k.typ(&mut p, "x");
        k.key(&mut p, KeyCode::Enter);
        k.key(&mut p, KeyCode::Down);
        k.render(&mut p, 100, 30);
    }
    // connections: the owning pid of System (4) is never offered for killing
    let mut p = quiet();
    odd_lists(&mut p);
    k.key(&mut p, KeyCode::Char('6'));
    k.key(&mut p, KeyCode::Char('G'));
    k.key(&mut p, KeyCode::Char('x'));
    assert!(p.pending_kill.is_none(), "pid 4 offered: {:?}", p.pending_kill);
}
