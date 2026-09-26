use super::probes::{self, Conn, Info, Section, Service, StartupItem};
use super::sampler::{self, DiskRow, Gpu, GpuState, Proc, Snap};
use super::*;
use crate::testkit::Kit;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

const NAMES: [&str; 14] = ["System", "Registry", "smss.exe", "csrss.exe", "wininit.exe", "services.exe", "lsass.exe", "svchost.exe", "dwm.exe", "chrome.exe", "oriel.exe", "python.exe", "explorer.exe", "Code.exe"];

/// Parent (index into the process list) of each of the first 14 processes.
const PARENT: [Option<usize>; 14] = [None, Some(0), Some(0), None, None, Some(4), Some(4), Some(5), Some(3), Some(12), Some(13), Some(13), None, Some(12)];

/// A fixed, nest-like machine so snapshots are deterministic.
fn fake_snap() -> Snap {
    let wave = |n: usize, f: &dyn Fn(usize) -> f64| -> VecDeque<f64> { (0..n).map(f).collect() };
    let pid = |i: usize| 4 + i as u32 * 4;
    let procs: Vec<Proc> = (0..702)
        .map(|i| {
            let name = NAMES[i % NAMES.len()];
            let parent = if i < 14 {
                PARENT[i]
            } else {
                Some(match name {
                    "svchost.exe" => 5,
                    "chrome.exe" => 9,
                    "Code.exe" | "oriel.exe" => 12,
                    "python.exe" => 13,
                    _ => 7,
                })
            };
            Proc {
                pid: pid(i),
                name: name.to_string(),
                cpu: ((i * 37) % 23) as f32 * 0.4,
                mem: ((i * 7919) % 400) as u64 * 1_100_000 + 900_000,
                threads: ((i * 13) % 60) as u32 + 1,
                user: Arc::from(if i % 5 == 0 { "SYSTEM" } else if i % 3 == 0 { "Le1f" } else { "" }),
                ppid: parent.map(pid).unwrap_or(0),
                start: if i < 14 { 0 } else { i as u64 },
                read: if i % 4 == 0 { ((i * 131) % 97) as f64 * 21_000.0 } else { 0.0 },
                write: if i % 6 == 0 { ((i * 71) % 89) as f64 * 9_000.0 } else { 0.0 },
            }
        })
        .collect();
    let cores = 20;
    Snap {
        seq: 1,
        cpu_hist: (0..120).map(|i| 12.0 + 10.0 * ((i as f32) * 0.3).sin().abs() + (i % 7) as f32).collect(),
        cores: (0..cores).map(|i| ((i * 17) % 90) as f32).collect(),
        core_hist: (0..cores).map(|c| (0..90).map(|i| (((i * (c + 3)) % 37) as f32 * 1.4 + (c * 4) as f32).min(100.0)).collect()).collect(),
        core_mhz: vec![3300; cores],
        ghz: 3.3,
        ram_used: 43_700_000_000,
        ram_total: 68_700_000_000,
        ram_hist: (0..120).map(|i| 60.0 + (i % 5) as f32 + (i as f32 / 20.0)).collect(),
        disks: vec![
            DiskRow { label: "C:".into(), total: 1_000_000_000_000, free: 30_400_000_000 },
            DiskRow { label: "D:".into(), total: 2_000_000_000_000, free: 1_200_000_000_000 },
            DiskRow { label: "E:".into(), total: 500_000_000_000, free: 12_000_000_000 },
        ],
        net_down: wave(60, &|i| ((i * 31) % 17) as f64 * 1200.0),
        net_up: wave(60, &|i| ((i * 11) % 13) as f64 * 500.0),
        disk_r: wave(60, &|i| ((i * 7) % 19) as f64 * 9000.0),
        disk_w: wave(60, &|i| ((i * 5) % 23) as f64 * 15000.0 + if i == 59 { 2e6 } else { 0.0 }),
        threads: procs.iter().map(|p| p.threads as u64).sum(),
        procs,
        sample_ms: 0.0,
    }
}

fn fake_pane() -> System {
    let mut p = System::new();
    p.started = true; // no sampler threads: fixed data only
    p.probes_live = false; // and no probe threads
    p.snap = Arc::new(fake_snap());
    p.gpu = GpuState::Ok(Gpu { name: "NVIDIA GeForce RTX 2080 Ti".into(), util: 80.0, used: 10_650.0, total: 11_264.0, temp: 84.0, power: 178.0, limit: 260.0, fan: 65.0 });
    p.gpu_hist = (0..60).map(|i| 40.0 + ((i * 13) % 50) as f32).collect();
    p.startup = Some(Arc::new(vec![
        StartupItem { name: "Steam".into(), command: r#""C:\Program Files (x86)\Steam\steam.exe" -silent"#.into(), location: "HKCU Run".into(), enabled: Some(true) },
        StartupItem { name: "Discord".into(), command: r#""C:\Users\me\AppData\Local\Discord\Update.exe" --processStart Discord.exe"#.into(), location: "HKCU Run".into(), enabled: Some(false) },
        StartupItem { name: "SecurityHealth".into(), command: r"%windir%\system32\SecurityHealthSystray.exe".into(), location: "HKLM Run".into(), enabled: Some(true) },
        StartupItem { name: "Everything".into(), command: r#""C:\Program Files\Everything\Everything.exe" -startup"#.into(), location: "HKLM Run".into(), enabled: Some(true) },
        StartupItem { name: "Adobe CCXProcess".into(), command: r"C:\Program Files (x86)\Adobe\Adobe Creative Cloud Experience\CCXProcess.exe".into(), location: "HKLM Run (32-bit)".into(), enabled: Some(false) },
        StartupItem { name: "Syncthing".into(), command: r"C:\Users\me\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\Syncthing.lnk".into(), location: "startup folder".into(), enabled: Some(true) },
    ]));
    let svc = |name: &str, display: &str, state: &str, start: &str, pid: u32, desc: &str| Service { name: name.into(), display: display.into(), state: state.into(), start: start.into(), pid, desc: desc.into() };
    p.services = Some(Arc::new(vec![
        svc("AudioSrv", "Windows Audio", "running", "auto", 3120, "Manages audio for Windows-based programs."),
        svc("BITS", "Background Intelligent Transfer Service", "stopped", "manual", 0, "Transfers files in the background using idle network bandwidth."),
        svc("Dhcp", "DHCP Client", "running", "auto", 1780, "Registers and updates IP addresses and DNS records for this computer."),
        svc("Spooler", "Print Spooler", "running", "auto", 4410, "This service spools print jobs and handles interaction with the printer."),
        svc("wuauserv", "Windows Update", "starting", "manual", 0, "Enables the detection, download, and installation of updates."),
        svc("Fax", "Fax", "stopped", "disabled", 0, "Enables you to send and receive faxes."),
        svc("nginx", "A high performance web server", "failed", "enabled", 0, ""),
    ]));
    let conn = |proto: &str, local: &str, remote: &str, state: &str, pid: u32, process: &str| Conn { proto: proto.into(), local: local.into(), remote: remote.into(), state: state.into(), pid, process: process.into() };
    p.conns = Some(Arc::new(vec![
        conn("tcp", "192.168.1.20:52144", "142.250.74.110:443", "established", 40, "chrome.exe"),
        conn("tcp", "192.168.1.20:52150", "140.82.112.25:443", "established", 40, "chrome.exe"),
        conn("tcp", "192.168.1.20:52201", "162.159.135.232:443", "established", 96, "Discord.exe"),
        conn("tcp", "0.0.0.0:135", "0.0.0.0:0", "listening", 1968, "svchost.exe"),
        conn("tcp", "[::]:445", "[::]:0", "listening", 4, "System"),
        conn("udp", "0.0.0.0:5353", "*:*", "", 2240, "svchost.exe"),
        conn("tcp", "192.168.1.20:52099", "20.42.73.29:443", "time wait", 0, ""),
    ]));
    p.info = Some(Arc::new(Info {
        boot: 0,
        sections: vec![
            Section { icon: "window", title: "os".into(), rows: vec![("system".into(), "Windows 11 Pro 24H2".into()), ("kernel".into(), "10.0.26200".into()), ("hostname".into(), "desk".into()), ("arch".into(), "x86_64".into())] },
            Section { icon: "system", title: "cpu".into(), rows: vec![("model".into(), "Intel(R) Core(TM) i9-7900X CPU @ 3.30GHz".into()), ("cores".into(), "10 cores · 20 threads".into()), ("base clock".into(), "3.31 GHz".into())] },
            Section { icon: "chart", title: "memory".into(), rows: vec![("installed".into(), "64.0 GB".into())] },
            Section { icon: "gauge", title: "gpu".into(), rows: vec![("NVIDIA GeForce RTX 2080 Ti".into(), "11 GB VRAM · driver 596.49".into())] },
            Section { icon: "package", title: "board".into(), rows: vec![("motherboard".into(), "Gigabyte X299 DESIGNARE EX-CF".into()), ("bios".into(), "American Megatrends Inc. F7h · 12/06/2021".into())] },
            Section { icon: "storage", title: "disks".into(), rows: vec![("drive 0".into(), "NVMe Samsung SSD 960".into()), ("C:".into(), "931.5 GB · NTFS · ssd · 28.3 GB free".into())] },
        ],
    }));
    for i in 0..3 {
        p.rebuild_list(i);
    }
    p.rebuild();
    p
}

#[test]
fn system_fake_snapshot() {
    let mut k = Kit::new();
    let mut p = fake_pane();
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-summary.html");
    println!("{out}");
    assert!(out.contains("20 threads · 3.3 GHz"));
    assert!(out.contains("RTX 2080 Ti"));
    assert!(out.contains("84°C"));
    assert!(out.contains("GB free"));
    assert!(out.contains("702 processes · sorted by cpu"));
    assert!(out.contains("read/s") && out.contains("write/s"));
    assert!(out.contains("c/m/r/w/n sort cpu·memory·read·write·name"));
    let src = include_str!("../system.rs").to_lowercase() + &(include_str!("sampler.rs").to_string() + include_str!("probes.rs") + include_str!("views.rs")).to_lowercase();
    assert!(!src.contains(&["wr", "en"].concat()) && !src.contains(&["train", "ing"].concat()));
}

#[test]
fn system_views_keys_and_side() {
    let mut k = Kit::new();
    let mut p = fake_pane();
    let side = k.render_side(&mut p, 34, 10);
    println!("{side}");
    for (_, _, label) in VIEWS {
        assert!(side.contains(label), "{label} in the sidebar");
    }
    assert!(side.contains("7"));
    k.key(&mut p, KeyCode::Char('3'));
    assert_eq!(p.view, View::Performance);
    k.key(&mut p, KeyCode::Tab);
    assert_eq!(p.view, View::Startup);
    k.key(&mut p, KeyCode::BackTab);
    k.key(&mut p, KeyCode::BackTab);
    assert_eq!(p.view, View::Processes);
    k.key(&mut p, KeyCode::Char('7'));
    k.key(&mut p, KeyCode::Tab);
    assert_eq!(p.view, View::Summary, "tab wraps around");
    // click "services" (row 4 of the side area)
    let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
    let mut acts = vec![];
    {
        let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &k.tx, actions: &mut acts, focused: true, time: 0.0 };
        p.side_mouse(click(3, 4), Rect::new(0, 0, 34, 10), &mut cx);
    }
    assert_eq!(p.view, View::Services);
}

#[test]
fn system_sort_filter_select() {
    let mut k = Kit::new();
    let mut p = fake_pane();
    k.render(&mut p, 150, 44);
    k.key(&mut p, KeyCode::Char('m'));
    assert_eq!(p.sort, Sort::Mem);
    let mems: Vec<u64> = p.rows.iter().map(|&i| p.snap.procs[i].mem).collect();
    assert!(mems.windows(2).all(|w| w[0] >= w[1]));
    k.key(&mut p, KeyCode::Char('r'));
    let reads: Vec<f64> = p.rows.iter().map(|&i| p.snap.procs[i].read).collect();
    assert!(reads.windows(2).all(|w| w[0] >= w[1]) && reads[0] > 0.0);
    k.key(&mut p, KeyCode::Char('w'));
    assert!(p.subtitle().unwrap().ends_with("sorted by disk write"));
    k.key(&mut p, KeyCode::Char('n'));
    assert!(p.subtitle().unwrap().ends_with("sorted by name"));
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "dwm");
    assert!(p.rows.iter().all(|&i| p.snap.procs[i].name == "dwm.exe"));
    let out = k.render(&mut p, 150, 44);
    assert!(out.contains("dwm"));
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
    // click the "cpu %" header to sort by it
    k.render(&mut p, 150, 44);
    let (x0, _, _) = *p.head_hits.iter().find(|h| h.2 == Sort::Cpu).unwrap();
    let click = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x0 + 1, row: p.table_head.y, modifiers: KeyModifiers::NONE };
    k.mouse(&mut p, click, Rect::new(0, 0, 150, 44));
    assert_eq!(p.sort, Sort::Cpu);
    // click the 4th row of the table
    k.key(&mut p, KeyCode::Char('g'));
    k.render(&mut p, 150, 44);
    let (x, y) = (p.table_body.x + 3, p.table_body.y + 3);
    k.mouse(&mut p, MouseEvent { column: x, row: y, ..click }, Rect::new(0, 0, 150, 44));
    assert_eq!(p.sel, 3);
}

#[test]
fn system_process_tree() {
    let mut k = Kit::new();
    let mut p = fake_pane();
    k.key(&mut p, KeyCode::Char('2'));
    k.key(&mut p, KeyCode::Char('t'));
    assert!(p.tree && p.tree_rows.len() == p.rows.len() && p.rows.len() == 702);
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-processes.html");
    println!("{out}");
    assert!(out.contains("├─") && out.contains("▾ "), "tree guides drawn");
    assert!(p.subtitle().unwrap().ends_with("· tree"));
    // every child sits below its parent
    let pos: HashMap<u32, usize> = p.rows.iter().enumerate().map(|(n, &i)| (p.snap.procs[i].pid, n)).collect();
    for (n, &i) in p.rows.iter().enumerate() {
        if let Some(&pn) = pos.get(&p.snap.procs[i].ppid) {
            assert!(pn < n);
        }
    }
    // fold the first row with children
    let at = p.tree_rows.iter().position(|m| m.kids).unwrap();
    p.sel = at;
    let before = p.rows.len();
    assert!(k.key(&mut p, KeyCode::Left));
    assert!(p.tree_rows[p.sel].folded && p.rows.len() < before);
    let out = k.render(&mut p, 150, 44);
    assert!(out.contains("▸ "));
    assert!(k.key(&mut p, KeyCode::Right));
    assert_eq!(p.rows.len(), before);
    // left on a child jumps to its parent
    k.key(&mut p, KeyCode::Down);
    let child_depth = p.tree_rows[p.sel].depth;
    k.key(&mut p, KeyCode::Left);
    if child_depth > 0 && !p.tree_rows[p.sel].kids {
        assert_eq!(p.tree_rows[p.sel].depth + 1, child_depth);
    }
    // filtering keeps ancestors
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "python");
    let names: HashSet<&str> = p.rows.iter().map(|&i| p.snap.procs[i].name.as_str()).collect();
    assert!(names.contains("python.exe") && names.contains("Code.exe") && names.contains("explorer.exe"));
    k.key(&mut p, KeyCode::Esc);
    k.key(&mut p, KeyCode::Char('t'));
    assert!(!p.tree && p.tree_rows.is_empty());
}

#[test]
fn system_other_views_snapshots() {
    let mut k = Kit::new();
    let mut p = fake_pane();
    k.key(&mut p, KeyCode::Char('3'));
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-performance.html");
    println!("{out}");
    assert!(out.contains("logical processors · 20"));
    assert!(out.contains("cpu 19"));
    assert!(out.contains("gpu · 2 min") && out.contains("memory · 2 min"));

    k.key(&mut p, KeyCode::Char('4'));
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-startup.html");
    println!("{out}");
    assert!(out.contains("6 startup entries · 4 on · 2 off"));
    assert!(out.contains("HKLM Run (32-bit)"));
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "off");
    assert_eq!(p.lists[0].rows.len(), 2);
    k.key(&mut p, KeyCode::Esc);

    k.key(&mut p, KeyCode::Char('5'));
    k.key(&mut p, KeyCode::Down);
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-services.html");
    println!("{out}");
    assert!(out.contains("7 services · 3 running · 3 stopped · 1 failed"));
    assert!(out.contains("idle network bandwidth"), "detail line shows the selected description");
    k.key(&mut p, KeyCode::Char('/'));
    k.typ(&mut p, "running");
    assert_eq!(p.lists[1].rows.len(), 3);
    k.key(&mut p, KeyCode::Enter);

    k.key(&mut p, KeyCode::Char('6'));
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-connections.html");
    println!("{out}");
    assert!(out.contains("7 connections · 3 established · 2 listening"));
    assert!(out.contains("most active  chrome.exe 2"));

    k.key(&mut p, KeyCode::Char('7'));
    let out = k.render_html(&mut p, 150, 44, "target/snap/system-info.html");
    println!("{out}");
    assert!(out.contains("i9-7900X") && out.contains("X299") && out.contains("in use"));
}

/// Every view at a handful of sizes, tiny ones included: nothing may panic or spill.
#[test]
fn system_all_sizes() {
    let mut k = Kit::new();
    let mut p = fake_pane();
    for key in ['1', '2', '3', '4', '5', '6', '7'] {
        k.key(&mut p, KeyCode::Char(key));
        if key == '2' {
            k.key(&mut p, KeyCode::Char('t'));
        }
        for (w, h) in [(20, 6), (40, 12), (60, 20), (80, 24), (100, 30), (220, 64)] {
            k.render(&mut p, w, h);
            k.render_side(&mut p, 10, 3);
        }
    }
    // empty data (nothing collected yet) too
    let mut p = System::new();
    p.started = true;
    p.probes_live = false;
    for key in ['1', '2', '3', '4', '5', '6', '7'] {
        k.key(&mut p, KeyCode::Char(key));
        let out = k.render(&mut p, 100, 30);
        if key == '5' {
            assert!(out.contains("reading services…"));
        }
    }
}

#[test]
fn system_parsers() {
    let reg = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    Steam    REG_SZ    \"C:\\Steam\\steam.exe\" -silent\r\n    electron.app.Twinkle Tray    REG_SZ    \"C:\\tt.exe\"\r\n    Empty    REG_SZ    \r\n\r\n";
    let v = probes::parse_reg(reg);
    assert_eq!(v.len(), 3);
    assert_eq!(v[1].1, "electron.app.Twinkle Tray");
    assert_eq!(v[0].3, "\"C:\\Steam\\steam.exe\" -silent");
    assert_eq!(v[2].3, "");
    assert_eq!(probes::approved_on("020000000000000000000000"), Some(true));
    assert_eq!(probes::approved_on("03000000E177555C807EDC01"), Some(false));

    let ns = "\nActive Connections\n\n  Proto  Local Address          Foreign Address        State           PID\n  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1968\n  TCP    192.168.1.2:5000       1.2.3.4:443            ESTABLISHED     40\n  TCP    [::1]:9000             [::1]:52000            TIME_WAIT       0\n  UDP    0.0.0.0:53             *:*                                    4468\n";
    let c = probes::parse_netstat_win(ns);
    assert_eq!(c.len(), 4);
    assert_eq!((c[0].state.as_str(), c[0].pid), ("listening", 1968));
    assert_eq!(c[2].state, "time wait");
    assert_eq!((c[3].proto.as_str(), c[3].state.as_str(), c[3].pid), ("udp", "", 4468));

    let ss = "Netid State  Recv-Q Send-Q Local Address:Port  Peer Address:Port Process\nudp   UNCONN 0      0      0.0.0.0%lo:5353 0.0.0.0:*         users:((\"avahi-daemon\",pid=612,fd=12))\ntcp   ESTAB  0      0      192.168.1.5:22   192.168.1.9:50122 users:((\"sshd\",pid=812,fd=4),(\"sshd\",pid=900,fd=4))\ntcp   LISTEN 0      128    [::]:80          [::]:*\n";
    let c = probes::parse_ss(ss);
    assert_eq!(c.len(), 3);
    assert_eq!((c[0].process.as_str(), c[0].pid, c[0].state.as_str()), ("avahi-daemon", 612, ""));
    assert_eq!((c[1].process.as_str(), c[1].pid, c[1].state.as_str()), ("sshd", 812, "established"));
    assert_eq!((c[2].pid, c[2].state.as_str(), c[2].local.as_str()), (0, "listening", "[::]:80"));

    let nl = "Active Internet connections (servers and established)\nProto Recv-Q Send-Q Local Address           Foreign Address         State       PID/Program name\ntcp        0      0 0.0.0.0:22              0.0.0.0:*               LISTEN      812/sshd\nudp        0      0 0.0.0.0:68              0.0.0.0:*                           640/dhclient\ntcp        0      0 10.0.0.2:40000          1.1.1.1:443             ESTABLISHED 2000/Web Content\ntcp6       0      0 :::631                  :::*                    LISTEN      -\n";
    let c = probes::parse_netstat_linux(nl);
    assert_eq!(c.len(), 4);
    assert_eq!((c[0].state.as_str(), c[0].pid, c[0].process.as_str()), ("listening", 812, "sshd"));
    assert_eq!((c[1].state.as_str(), c[1].pid), ("", 640));
    assert_eq!(c[2].process, "Web Content");
    assert_eq!((c[3].pid, c[3].state.as_str()), (0, "listening"));

    let units = "ssh.service loaded active running OpenBSD Secure Shell server\n● nginx.service loaded failed failed A high performance web server\ncups.service loaded inactive dead CUPS Scheduler\nsetup.service loaded active exited Setup\n";
    let s = probes::parse_systemctl_units(units);
    assert_eq!(s.iter().map(|s| s.state.as_str()).collect::<Vec<_>>(), ["running", "failed", "stopped", "exited"]);
    assert_eq!((s[1].name.as_str(), s[1].display.as_str()), ("nginx", "A high performance web server"));
    let files = probes::parse_unit_files("ssh.service enabled enabled\ngetty@.service enabled enabled\n");
    assert_eq!(files[1], ("getty@.service".to_string(), "enabled".to_string()));

    let d = "[Desktop Entry]\nName=Syncthing\nName[de]=Sync\nExec=syncthing -no-browser\nX-GNOME-Autostart-enabled=false\n[Desktop Action x]\nName=Other\n";
    assert_eq!(probes::parse_desktop(d), Some(("Syncthing".into(), "syncthing -no-browser".into(), false)));
    assert_eq!(probes::parse_desktop("[Desktop Entry]\nExec=x\n"), None);
    assert_eq!(probes::parse_lspci("00:02.0 VGA compatible controller: Intel Corporation UHD Graphics 630 (rev 02)\n01:00.0 3D controller: NVIDIA Corporation TU117M (rev a1)\n00:1f.3 Audio device: Intel\n"), ["Intel Corporation UHD Graphics 630", "NVIDIA Corporation TU117M"]);
    assert_eq!(probes::disk_model(r"SCSI\Disk&Ven_NVMe&Prod_Samsung_SSD_960\5&87ca3f&0&000000"), "NVMe Samsung SSD 960");

    // pids named from the snapshot, established first
    let snap = fake_snap();
    let c = probes::finish_conns(probes::parse_netstat_win(ns), &snap);
    assert_eq!(c[0].state, "established");
    assert_eq!(c[0].process, "chrome.exe"); // pid 40 = index 9
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
    let mut p = fake_pane();
    p.gpu = GpuState::Missing;
    let out = k.render(&mut p, 150, 44);
    assert!(!out.contains("gpu"), "gpu card hidden without nvidia-smi");
    k.key(&mut p, KeyCode::Char('3'));
    let out = k.render(&mut p, 150, 44);
    assert!(out.contains("memory · 2 min") && !out.contains("gpu ·"));
}

static KILLED: AtomicU32 = AtomicU32::new(0);
fn fake_kill(pid: u32) -> Result<(), String> {
    KILLED.store(pid, Ordering::SeqCst);
    Ok(())
}

#[test]
fn system_kill_confirm() {
    let mut k = Kit::new();
    let mut p = fake_pane();
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
    // from the connections view it asks about the owning process
    k.key(&mut p, KeyCode::Char('6'));
    k.key(&mut p, KeyCode::Char('k'));
    assert_eq!(p.pending_kill.as_ref().map(|k| k.0), Some(40));
    k.key(&mut p, KeyCode::Esc);
}

/// The real machine: background sampling, then time what the UI thread does per refresh. The probes run for
/// real too (read-only: registry queries, the service list, netstat). Opt-in: it reads the real machine and its
/// timing flakes under load. `cargo test system_live_and_timing -- --ignored --nocapture`
#[test]
#[ignore]
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
    assert!(p.snap.procs.iter().any(|x| x.ppid != 0), "parent pids");
    assert_eq!(p.snap.core_hist.len(), p.snap.cores.len());
    k.render_html(&mut p, 150, 44, "target/snap/system-live-summary.html");
    println!("sampler thread: {:.1} ms per sample, {n} processes", p.snap.sample_ms);

    let time = |k: &mut Kit, p: &mut System, label: &str| {
        let iters = 30;
        let t = Instant::now();
        for _ in 0..iters {
            p.seq = 0; // force the rebuild path every time, as if a new snapshot arrived
            p.probe_seq = u64::MAX;
            k.poll(p);
            k.render(p, 150, 44);
        }
        let per = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;
        println!("UI refresh {label}: {per:.2} ms avg over {iters}");
        per
    };
    let mut worst = time(&mut k, &mut p, "summary (poll + rebuild + render 150x44)");
    k.key(&mut p, KeyCode::Char('n'));
    worst = worst.max(time(&mut k, &mut p, "summary sorted by name"));
    k.key(&mut p, KeyCode::Char('2'));
    k.key(&mut p, KeyCode::Char('t'));
    worst = worst.max(time(&mut k, &mut p, "processes as a tree"));
    k.render_html(&mut p, 150, 44, "target/snap/system-live-processes.html");
    k.key(&mut p, KeyCode::Char('3'));
    worst = worst.max(time(&mut k, &mut p, "performance"));
    k.render_html(&mut p, 150, 44, "target/snap/system-live-performance.html");
    for (key, name, what) in [('4', "startup", "startup"), ('5', "services", "services"), ('6', "connections", "connections"), ('7', "info", "system info")] {
        k.key(&mut p, KeyCode::Char(key));
        let deadline = Instant::now() + Duration::from_secs(20);
        let loaded = |p: &System| match key {
            '4' => p.startup.is_some(),
            '5' => p.services.is_some(),
            '6' => p.conns.is_some(),
            _ => p.info.is_some(),
        };
        while !loaded(&p) && Instant::now() < deadline {
            k.wait_wake(&mut p, 100);
        }
        assert!(loaded(&p), "{what} never arrived");
        let took = p.probes.lock().took;
        println!("{what}: collected in {:.0} ms on a probe thread", took[match key { '4' => 0, '5' => 1, '6' => 2, _ => 3 }]);
        worst = worst.max(time(&mut k, &mut p, what));
        k.render_html(&mut p, 150, 44, &format!("target/snap/system-live-{name}.html"));
    }
    println!("first render {:.2} ms, worst view {worst:.2} ms", first_render.as_secs_f64() * 1000.0);
    assert!(p.services.as_ref().unwrap().len() > 5 && p.info.as_ref().unwrap().sections.len() >= 3);
    assert!(worst < 50.0);
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
}
