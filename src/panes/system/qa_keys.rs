//! QA key-mash for the system app (see src/qa_keys.rs). The kill button is swapped for a fake that only counts
//! (so `k` + `y` never touches a real process), probe threads are off, and the data is a fixed fake machine —
//! including the awkward parts of real Windows data: pid reuse that makes parent loops, a process that is its own
//! parent, orphans, empty names, zero totals.

use super::probes::{Conn, Info, Section, Service, StartupItem};
use super::sampler::{DiskRow, Gpu, GpuState, Proc, Snap};
use super::*;
use crate::qa_keys::{Ev, Opts, mash_pane};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering as AtOrd};

static KILLS: AtomicU32 = AtomicU32::new(0);

fn fake_kill(_pid: u32) -> Result<(), String> {
    KILLS.fetch_add(1, AtOrd::SeqCst);
    Err("qa: not really".into())
}

fn snap(n: usize, seq: u64) -> Snap {
    let names = ["System", "svchost.exe", "chrome.exe", "中文进程.exe", "", "a-very-long-process-name-that-never-seems-to-end-at-all.exe", "🙂.exe", "oriel.exe"];
    let mut procs: Vec<Proc> = (0..n)
        .map(|i| Proc {
            pid: 100 + i as u32 * 4,
            name: names[i % names.len()].to_string(),
            cpu: ((i * 37) % 23) as f32 * if i % 11 == 0 { 9.0 } else { 0.4 },
            mem: ((i * 7919) % 400) as u64 * 1_100_000,
            threads: (i % 60) as u32,
            user: Arc::from(if i % 5 == 0 { "SYSTEM" } else { "" }),
            ppid: if i == 0 { 0 } else { 100 + ((i.wrapping_mul(2_654_435_761) >> 7) % i) as u32 * 4 },
            start: i as u64,
            read: if i % 4 == 0 { (i * 1000) as f64 } else { 0.0 },
            write: if i % 6 == 0 { f64::MAX / 4.0 } else { 0.0 },
        })
        .collect();
    // pid reuse: a two-process loop, a process that is its own parent, and a parent that doesn't exist
    let p = |pid: u32, ppid: u32, start: u64| Proc { pid, name: format!("loop{pid}"), cpu: 1.0, mem: 1, threads: 1, user: Arc::from(""), ppid, start, read: 0.0, write: 0.0 };
    procs.extend([p(90_001, 90_002, 5), p(90_002, 90_001, 4), p(90_003, 90_003, 1), p(90_004, 12_345_678, 9)]);
    let wave = |f: &dyn Fn(usize) -> f64| -> VecDeque<f64> { (0..60).map(f).collect() };
    Snap {
        seq,
        cpu_hist: (0..120).map(|i| (i % 101) as f32).collect(),
        cores: (0..20).map(|i| (i * 17 % 101) as f32).collect(),
        core_hist: (0..20).map(|c| (0..90).map(|i| ((i * (c + 3)) % 101) as f32).collect()).collect(),
        core_mhz: (0..20).map(|i| if i % 3 == 0 { 0 } else { 3300 }).collect(),
        ghz: 3.3,
        ram_used: 43_700_000_000,
        ram_total: if seq % 2 == 0 { 68_700_000_000 } else { 0 }, // 0: a sampler that couldn't read it
        ram_hist: (0..120).map(|i| (i % 100) as f32).collect(),
        disks: vec![DiskRow { label: "C:".into(), total: 1_000_000_000_000, free: 30_400_000_000 }, DiskRow { label: "Z:".into(), total: 0, free: 0 }],
        net_down: wave(&|i| (i * 1200) as f64),
        net_up: wave(&|i| if i % 7 == 0 { f64::MAX / 8.0 } else { 0.0 }),
        disk_r: wave(&|i| (i * 90_000) as f64),
        disk_w: VecDeque::new(),
        threads: procs.iter().map(|p| p.threads as u64).sum(),
        procs,
        sample_ms: 1.0,
    }
}

/// A System with fixed data and a fake kill. `rich` fills the startup / services / connections / info views too.
pub(crate) fn safe(rich: bool) -> System {
    let mut p = System::new();
    p.started = true; // no sampler thread: the data below is all there is
    p.probes_live = false; // and no probe threads (startup items, services, connections, info)
    p.killer = fake_kill;
    if rich {
        p.snap = Arc::new(snap(400, 1));
        p.gpu = GpuState::Ok(Gpu { name: "GPU 中文".into(), util: 101.0, used: 12_000.0, total: 11_264.0, temp: 84.0, power: 0.0, limit: 0.0, fan: -1.0 });
        p.gpu_hist = (0..60).map(|i| (i * 13 % 120) as f32).collect();
        p.startup = Some(Arc::new(
            (0..30).map(|i| StartupItem { name: format!("item {i} 🙂"), command: format!(r#""C:\Program Files\x{i}\x.exe" --flag {}"#, "a".repeat(i * 7)), location: "HKCU Run".into(), enabled: [Some(true), Some(false), None][i % 3] }).collect(),
        ));
        p.services = Some(Arc::new(
            (0..50).map(|i| Service { name: format!("svc{i}"), display: format!("Service number {i} ✦"), state: ["running", "stopped", "starting", "failed", ""][i % 5].into(), start: "auto".into(), pid: i as u32, desc: "x".repeat(i * 3) }).collect(),
        ));
        p.conns = Some(Arc::new(
            (0..40).map(|i| Conn { proto: if i % 2 == 0 { "tcp" } else { "udp" }.into(), local: format!("192.168.1.20:{}", 50000 + i), remote: "[::]:0".into(), state: ["established", "listening", "", "time wait"][i % 4].into(), pid: if i % 3 == 0 { 0 } else { 100 + i as u32 * 4 }, process: if i % 5 == 0 { String::new() } else { "chrome.exe".into() } }).collect(),
        ));
        p.info = Some(Arc::new(Info {
            boot: 0,
            sections: vec![
                Section { icon: "window", title: "os".into(), rows: vec![("system".into(), "Windows".into()), ("".into(), "".into())] },
                Section { icon: "system", title: "cpu 中文".into(), rows: (0..30).map(|i| (format!("row {i}"), "v".repeat(i * 5))).collect() },
                Section { icon: "gauge", title: "empty".into(), rows: vec![] },
            ],
        }));
        for i in 0..3 {
            p.rebuild_list(i);
        }
        p.rebuild();
    }
    p
}

fn guard(p: &mut System, ev: &mut Ev) -> bool {
    // belt and braces: the killer is fake anyway, but a y after k is what would kill
    if let Ev::Key(k) = ev {
        if matches!(k.code, KeyCode::Char('y' | 'Y')) && p.pending_kill.is_some() {
            assert!(std::ptr::fn_addr_eq(p.killer, fake_kill as fn(u32) -> Result<(), String>), "the real killer is wired up");
        }
    }
    true
}

#[test]
fn qa_keys_system_rich() {
    mash_pane("system-rich", Opts { snap: Some("system".into()), ..Default::default() }, |_| safe(true), guard);
}

#[test]
fn qa_keys_system_empty() {
    mash_pane("system-empty", Opts { keys: crate::qa_keys::keys_for(1500), ..Default::default() }, |_| safe(false), guard);
}

/// New snapshots arrive while you work: the list shrinks and grows under the cursor, the tree changes shape.
#[test]
fn qa_keys_system_changing_data() {
    let mut n = 0u64;
    mash_pane("system-changing", Opts::default(), |_| safe(true), move |p, ev| {
        n += 1;
        if n % 40 == 0 {
            let size = [0, 3, 400, 50, 1][(n / 40) as usize % 5];
            p.snap = Arc::new(snap(size, n));
            p.seq = 0;
            p.rebuild();
        }
        guard(p, ev)
    });
}
