use super::sampler::{self, DiskRow, Eval, Gpu, GpuState, Proc, Progress, Snap, Train};
use super::*;
use crate::testkit::Kit;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

/// A fixed, nest-like machine so snapshots are deterministic.
fn fake_snap(train: bool) -> Snap {
    let wave = |n: usize, f: &dyn Fn(usize) -> f64| -> VecDeque<f64> { (0..n).map(f).collect() };
    let names = ["System", "Registry", "smss.exe", "csrss.exe", "wininit.exe", "services.exe", "lsass.exe", "svchost.exe", "dwm.exe", "chrome.exe", "oriel.exe", "python.exe", "explorer.exe", "Code.exe"];
    let procs: Vec<Proc> = (0..702)
        .map(|i| Proc {
            pid: 4 + i as u32 * 4,
            name: names[i % names.len()].to_string(),
            cpu: ((i * 37) % 23) as f32 * 0.4,
            mem: ((i * 7919) % 400) as u64 * 1_100_000 + 900_000,
            threads: ((i * 13) % 60) as u32 + 1,
            user: Arc::from(if i % 5 == 0 { "SYSTEM" } else if i % 3 == 0 { "Le1f" } else { "" }),
        })
        .collect();
    Snap {
        seq: 1,
        cpu_hist: (0..60).map(|i| 12.0 + 10.0 * ((i as f32) * 0.3).sin().abs() + (i % 7) as f32).collect(),
        cores: (0..20).map(|i| ((i * 17) % 90) as f32).collect(),
        ghz: 3.3,
        ram_used: 43_700_000_000,
        ram_total: 68_700_000_000,
        ram_hist: (0..60).map(|i| 60.0 + (i % 5) as f32).collect(),
        disks: vec![
            DiskRow { label: "C:".into(), total: 1_000_000_000_000, free: 30_400_000_000 },
            DiskRow { label: "D:".into(), total: 2_000_000_000_000, free: 1_200_000_000_000 },
            DiskRow { label: "E:".into(), total: 500_000_000_000, free: 12_000_000_000 },
        ],
        net_down: wave(60, &|i| ((i * 31) % 17) as f64 * 1200.0),
        net_up: wave(60, &|i| ((i * 11) % 13) as f64 * 500.0),
        disk_r: wave(60, &|i| ((i * 7) % 19) as f64 * 9000.0),
        disk_w: wave(60, &|i| ((i * 5) % 23) as f64 * 15000.0 + if i == 59 { 2e6 } else { 0.0 }),
        procs,
        train: train.then(|| Train {
            mode: "pretrain".into(),
            running: true,
            pid: Some(4242),
            progress: Some(Progress { step: 5120, total: 8392, loss: 3.412, tok_s: 48_300.0, eta: 3.4 * 3600.0 }),
            evals: Arc::new((0..20).map(|i| Eval { step: 250 * (i + 1), val: 4.7 - (i as f64).sqrt() * 0.3, time: i as f64 * 600.0 }).collect()),
            samples: Arc::new(vec![("The sun is".into(), " a bright star that warms the day.".into()), ("Once upon a time, there was a".into(), " small bird who wanted to fly.".into())]),
        }),
        sample_ms: 0.0,
    }
}

fn fake_pane(train: bool) -> System {
    let mut p = System::new();
    p.started = true; // no sampler threads: fixed data only
    p.snap = Arc::new(fake_snap(train));
    p.gpu = GpuState::Ok(Gpu { name: "NVIDIA GeForce RTX 2080 Ti".into(), util: 80.0, used: 10_650.0, total: 11_264.0, temp: 84.0, power: 178.0, limit: 260.0, fan: 65.0 });
    p.gpu_hist = (0..60).map(|i| 40.0 + ((i * 13) % 50) as f32).collect();
    p.rebuild();
    p
}

#[test]
fn system_fake_snapshot() {
    let mut k = Kit::new();
    let mut p = fake_pane(false);
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-fake.html");
    println!("{out}");
    assert!(out.contains("cpu"));
    assert!(out.contains("20 threads · 3.3 GHz"));
    assert!(out.contains("RTX 2080 Ti"));
    assert!(out.contains("84°C"));
    assert!(out.contains("GB free"));
    assert!(out.contains("702 processes · sorted by cpu"));
    assert!(!out.contains("wren training"), "training card hidden when nothing trains");
    assert!(out.contains("c/m/n sort cpu·memory·name · k kill · / filter"));
}

#[test]
fn system_training_card() {
    let mut k = Kit::new();
    let mut p = fake_pane(true);
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-train.html");
    assert!(out.contains("wren training"));
    assert!(out.contains("● running pretrain"));
    assert!(out.contains("step 5120/8392"));
    k.key(&mut p, KeyCode::Char('t'));
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-train-big.html");
    assert!(out.contains("latest samples"));
    assert!(!out.contains("pid    name"));
}

#[test]
fn system_sort_filter_select() {
    let mut k = Kit::new();
    let mut p = fake_pane(false);
    k.render(&mut p, 150, 44);
    k.key(&mut p, KeyCode::Char('m'));
    assert_eq!(p.sort, Sort::Mem);
    let mems: Vec<u64> = p.rows.iter().map(|&i| p.snap.procs[i].mem).collect();
    assert!(mems.windows(2).all(|w| w[0] >= w[1]));
    k.key(&mut p, KeyCode::Char('n'));
    assert!(p.subtitle().unwrap().ends_with("sorted by name"));
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "dwm");
    assert!(p.rows.iter().all(|&i| p.snap.procs[i].name == "dwm.exe"));
    let out = k.render(&mut p, 150, 44);
    assert!(out.contains("dwm"));
    assert!(p.subtitle().unwrap().starts_with(&format!("{} processes", p.rows.len())));
    k.key(&mut p, KeyCode::Enter);
    assert!(!p.filtering);
    k.key(&mut p, KeyCode::Char('j'));
    k.key(&mut p, KeyCode::Char('j'));
    assert_eq!(p.sel, 2);
    k.key(&mut p, KeyCode::Up);
    assert_eq!(p.sel, 1);
    k.key(&mut p, KeyCode::Esc); // clears the filter, keeps the cursor on the same process
    assert!(p.filter.is_empty());
    assert_eq!(p.selected().unwrap().name, "dwm.exe");
    k.render_html(&mut p, 150, 44, "target/snap/system-sorted-name.html");
}

static KILLED: AtomicU32 = AtomicU32::new(0);
fn fake_kill(pid: u32) -> Result<(), String> {
    KILLED.store(pid, Ordering::SeqCst);
    Ok(())
}

#[test]
fn system_kill_confirm() {
    let mut k = Kit::new();
    let mut p = fake_pane(false);
    p.killer = fake_kill; // never kill a real process in tests
    k.render(&mut p, 150, 44);
    k.key(&mut p, KeyCode::Char('j'));
    let pid = p.selected().unwrap().pid;
    k.key(&mut p, KeyCode::Char('k'));
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-kill.html");
    assert!(out.contains(&format!("(pid {pid})?  y = yes · esc = no")));
    k.key(&mut p, KeyCode::Esc);
    assert!(p.pending_kill.is_none());
    assert!(k.notices().iter().any(|n| n == "left it alone"));
    assert_eq!(KILLED.load(Ordering::SeqCst), 0);
    k.key(&mut p, KeyCode::Char('k'));
    k.key(&mut p, KeyCode::Char('y'));
    k.wait_wake(&mut p, 300);
    assert_eq!(KILLED.load(Ordering::SeqCst), pid);
    assert!(k.notices().iter().any(|n| n.starts_with("sent terminate")));
}

#[test]
fn system_side_and_mouse() {
    let mut k = Kit::new();
    let mut p = fake_pane(false);
    let side = k.render_side(&mut p, 34, 10);
    println!("{side}");
    for s in ["overview", "sort by cpu", "sort by memory", "sort by name", "training"] {
        assert!(side.contains(s));
    }
    // click "sort by memory" (row 2 of the side area)
    let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
    let mut cx_actions = vec![];
    {
        let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut cx_actions, focused: true, time: 0.0 };
        p.side_mouse(click(3, 2), Rect::new(0, 0, 34, 10), &mut cx);
    }
    assert_eq!(p.sort, Sort::Mem);
    assert_eq!(p.view, View::Sort(Sort::Mem));
    // click the 4th row of the table
    k.render(&mut p, 150, 44);
    let (x, y) = (p.table_body.x + 3, p.table_body.y + 3);
    k.mouse(&mut p, click(x, y), Rect::new(0, 0, 150, 44));
    assert_eq!(p.sel, 3);
    assert_eq!(p.badge().unwrap(), format!("cpu {:.0}%", p.snap.cpu_hist.back().unwrap()));
}

#[test]
fn system_gpu_parse() {
    match sampler::parse_gpu("NVIDIA GeForce RTX 2080 Ti, 80, 10650, 11264, 84, 178.52, 260.00, 65\n") {
        GpuState::Ok(g) => {
            assert_eq!(g.util, 80.0);
            assert_eq!(g.temp, 84.0);
            assert!((g.power - 178.52).abs() < 0.01);
        }
        _ => panic!("parse failed"),
    }
    assert!(matches!(sampler::parse_gpu("[N/A]"), GpuState::Failed));
    let mut k = Kit::new();
    let mut p = fake_pane(false);
    p.gpu = GpuState::Missing;
    let out = k.render(&mut p, 150, 44);
    assert!(!out.contains("gpu"), "gpu card hidden without nvidia-smi");
}

/// The real machine: background sampling, then time what the UI thread does per refresh.
#[test]
fn system_live_and_timing() {
    let mut k = Kit::new();
    let mut p = System::new();
    let t0 = Instant::now();
    k.render(&mut p, 150, 44); // starts the sampler
    let first_render = t0.elapsed();
    // wait for two samples so per-process cpu % is real
    let deadline = Instant::now() + Duration::from_secs(6);
    while p.seq < 2 && Instant::now() < deadline {
        k.wait_wake(&mut p, 200);
    }
    assert!(p.seq >= 2, "sampler produced {} snapshots", p.seq);
    let n = p.snap.procs.len();
    assert!(n > 10, "only {n} processes");
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-main.html");
    println!("{out}");
    println!("sampler thread: {:.1} ms per sample, {n} processes", p.snap.sample_ms);

    // UI-thread cost of one refresh: take the snapshot + re-sort/filter + draw the whole pane
    let iters = 30;
    let t = Instant::now();
    for _ in 0..iters {
        p.seq = 0; // force the rebuild path every time, as if a new snapshot arrived
        k.poll(&mut p);
        k.render(&mut p, 150, 44);
    }
    let per = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    println!("UI refresh (poll + rebuild + render 150x44): {per:.2} ms avg over {iters}, first render {:.2} ms", first_render.as_secs_f64() * 1000.0);
    // name sort (the most expensive sort) too
    k.key(&mut p, KeyCode::Char('n'));
    let t = Instant::now();
    for _ in 0..iters {
        p.seq = 0;
        k.poll(&mut p);
        k.render(&mut p, 150, 44);
    }
    let per_name = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    println!("UI refresh sorted by name: {per_name:.2} ms avg");
    assert!(per < 50.0 && per_name < 50.0);
}

/// Where the sampler thread's time goes: `cargo test --release system_profile -- --ignored --nocapture`
#[test]
#[ignore]
fn system_profile_sampler() {
    use sysinfo::*;
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    let mut s = sampler::Sampler::new();
    for i in 0..4 {
        let t = Instant::now();
        let snap = s.sample();
        println!("sample {i}: {:.1} ms ({} procs)", ms(t), snap.procs.len());
        std::thread::sleep(Duration::from_millis(500));
    }
    let mut sys = System::new();
    for (name, kind) in [
        ("nothing", ProcessRefreshKind::nothing().without_tasks()),
        ("cpu", ProcessRefreshKind::nothing().with_cpu().without_tasks()),
        ("mem", ProcessRefreshKind::nothing().with_memory().without_tasks()),
        ("user", ProcessRefreshKind::nothing().with_user(UpdateKind::OnlyIfNotSet).without_tasks()),
    ] {
        for _ in 0..2 {
            let t = Instant::now();
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
            println!("procs {name}: {:.1} ms", ms(t));
        }
    }
    let mut d = Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing().with_storage().with_io_usage());
    let t = Instant::now();
    d.refresh_specifics(true, DiskRefreshKind::nothing().with_storage().with_io_usage());
    println!("disks: {:.1} ms", ms(t));
    let mut n = Networks::new_with_refreshed_list();
    let t = Instant::now();
    n.refresh(true);
    println!("nets: {:.1} ms", ms(t));
    let t = Instant::now();
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    println!("cpu+mem: {:.1} ms", ms(t));
}
