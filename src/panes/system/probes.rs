//! The slow, occasional data behind the startup / services / connections / system info views. Each is collected
//! on a short-lived background thread, only while its view is on screen, and handed to the UI behind an `Arc`
//! (the UI never runs a command or touches the filesystem). Commands spawn with CREATE_NO_WINDOW on Windows so
//! no console flashes. The parsers are plain functions over text so the tests can feed them canned output.

use super::sampler::Snap;
use crate::pane::Waker;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ data

#[derive(Clone, Debug, PartialEq)]
pub struct StartupItem {
    pub name: String,
    pub command: String,
    /// Where it's registered: "HKCU Run", "startup folder", "~/.config/autostart", "systemd --user"...
    pub location: String,
    /// Some(false) = present but switched off (Task Manager's "disabled", `Hidden=true`); None = unknown.
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    pub name: String,
    /// Windows display name / Linux description.
    pub display: String,
    /// "running", "stopped", "failed", "exited", "starting", "stopping", "paused", or whatever the OS said.
    pub state: String,
    /// "auto", "auto (delayed)", "manual", "disabled", "enabled", "static"... ("" = unknown)
    pub start: String,
    pub pid: u32,
    /// Windows: the service description (the display name says what it is on Linux).
    pub desc: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conn {
    pub proto: String,
    pub local: String,
    pub remote: String,
    /// "established", "listening", "time wait"... ("" for UDP)
    pub state: String,
    pub pid: u32,
    pub process: String,
}

#[derive(Clone, Debug, Default)]
pub struct Section {
    pub icon: &'static str,
    pub title: String,
    pub rows: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct Info {
    pub sections: Vec<Section>,
    /// Boot time, seconds since the Unix epoch (uptime is worked out when drawing).
    pub boot: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Startup = 0,
    Services = 1,
    Conns = 2,
    Info = 3,
}

#[derive(Default)]
pub struct Data {
    pub startup: Option<Arc<Vec<StartupItem>>>,
    pub services: Option<Arc<Vec<Service>>>,
    pub conns: Option<Arc<Vec<Conn>>>,
    pub info: Option<Arc<Info>>,
    /// Bumped whenever anything above changes.
    pub seq: u64,
    /// How long the last collection of each kind took (ms).
    pub took: [f64; 4],
    busy: [bool; 4],
    last: [Option<Instant>; 4],
}

#[derive(Default)]
pub struct Probes {
    data: Mutex<Data>,
}

impl Probes {
    pub fn lock(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Collect `kind` on a background thread, unless one is already running or the last result is younger than
    /// `every` (None = collect once). `snap` names the pids in the connection list.
    pub fn want(self: &Arc<Self>, kind: Kind, every: Option<Duration>, snap: Arc<Snap>, waker: Waker) {
        let k = kind as usize;
        let prev_services = {
            let mut d = self.lock();
            if d.busy[k] {
                return;
            }
            match (d.last[k], every) {
                (Some(_), None) => return,
                (Some(t), Some(e)) if t.elapsed() < e => return,
                _ => {}
            }
            d.busy[k] = true;
            d.services.clone()
        };
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("oriel-probe".into()).spawn(move || {
            let t0 = Instant::now();
            let mut out = Data::default();
            match kind {
                Kind::Startup => out.startup = Some(Arc::new(startup())),
                Kind::Services => out.services = Some(Arc::new(services(prev_services.as_ref().map(|v| v.as_slice())))),
                Kind::Conns => out.conns = Some(Arc::new(connections(&snap))),
                Kind::Info => out.info = Some(Arc::new(info())),
            }
            {
                let mut d = me.lock();
                match kind {
                    Kind::Startup => d.startup = out.startup,
                    Kind::Services => d.services = out.services,
                    Kind::Conns => d.conns = out.conns,
                    Kind::Info => d.info = out.info,
                }
                d.took[k] = t0.elapsed().as_secs_f64() * 1000.0;
                d.busy[k] = false;
                d.last[k] = Some(Instant::now());
                d.seq += 1;
            }
            waker.wake();
        });
        if spawned.is_err() {
            self.lock().busy[k] = false;
        }
    }
}

// ------------------------------------------------------------------ running commands

/// Run a program and return its stdout as text. No console window on Windows; stdin/stderr go nowhere.
pub fn run_cmd(prog: &str, args: &[&str]) -> std::io::Result<String> {
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output()?;
    Ok(decode(&out.stdout))
}

fn run(prog: &str, args: &[&str]) -> Option<String> {
    run_cmd(prog, args).ok().filter(|s| !s.trim().is_empty())
}

/// Console tools on Windows write the OEM code page when their output is a pipe; UTF-8 passes straight through.
#[cfg(windows)]
fn decode(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_string();
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MultiByteToWideChar(cp: u32, flags: u32, src: *const u8, n: i32, dst: *mut u16, cap: i32) -> i32;
    }
    const CP_OEMCP: u32 = 1;
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), b.len() as i32, std::ptr::null_mut(), 0) };
    if n <= 0 {
        return String::from_utf8_lossy(b).into_owned();
    }
    let mut w = vec![0u16; n as usize];
    unsafe { MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), b.len() as i32, w.as_mut_ptr(), n) };
    String::from_utf16_lossy(&w)
}
#[cfg(not(windows))]
fn decode(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ------------------------------------------------------------------ parsers (plain text in, rows out)

/// `reg query` output -> (key, value name, type, data). Value lines are indented 4 spaces and separated by
/// 4 spaces: `    Steam    REG_SZ    "C:\..\steam.exe" -silent` (names may contain single spaces).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_reg(text: &str) -> Vec<(String, String, String, String)> {
    let mut out = vec![];
    let mut key = String::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("HKEY_") {
            key = line.trim().to_string();
            continue;
        }
        let Some(rest) = line.strip_prefix("    ") else { continue };
        let Some(at) = rest.find("    REG_") else { continue };
        let name = rest[..at].to_string();
        let after = &rest[at + 4..];
        let (ty, data) = match after.find("    ") {
            Some(i) => (&after[..i], &after[i + 4..]),
            None => (after, ""),
        };
        out.push((key.clone(), name, ty.trim().to_string(), data.to_string()));
    }
    out
}

/// StartupApproved values: REG_BINARY whose first byte is even when the entry is on (02, 06) and odd when the
/// user switched it off in Task Manager (03, 07).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn approved_on(hex: &str) -> Option<bool> {
    let b = u8::from_str_radix(hex.trim().get(..2)?, 16).ok()?;
    Some(b & 1 == 0)
}

fn norm_state(s: &str) -> String {
    match s.to_ascii_uppercase().as_str() {
        "ESTAB" | "ESTABLISHED" => "established".into(),
        "LISTEN" | "LISTENING" => "listening".into(),
        "UNCONN" | "" => String::new(),
        other => other.to_ascii_lowercase().replace(['_', '-'], " "),
    }
}

/// `netstat -ano` (Windows): `  TCP    0.0.0.0:135    0.0.0.0:0    LISTENING    1968` / `  UDP    0.0.0.0:53    *:*    4468`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_netstat_win(text: &str) -> Vec<Conn> {
    let mut out = vec![];
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (state, pid) = match (f.first().map(|s| s.to_ascii_uppercase()).as_deref(), f.len()) {
            (Some("TCP"), 5) => (f[3], f[4]),
            (Some("UDP"), 4) => ("", f[3]),
            _ => continue,
        };
        out.push(Conn { proto: f[0].to_ascii_lowercase(), local: f[1].into(), remote: f[2].into(), state: norm_state(state), pid: pid.parse().unwrap_or(0), process: String::new() });
    }
    out
}

/// `ss -tunap` (Linux): `tcp   ESTAB  0  0  192.168.1.5:22  192.168.1.9:50122  users:(("sshd",pid=812,fd=4))`.
/// Without root, other users' sockets have no process column.
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_ss(text: &str) -> Vec<Conn> {
    let mut out = vec![];
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 || f[0].eq_ignore_ascii_case("netid") {
            continue;
        }
        let proto = f[0].to_ascii_lowercase();
        if !(proto.starts_with("tcp") || proto.starts_with("udp")) {
            continue;
        }
        let rest = f[6..].join(" ");
        let (mut pid, mut process) = (0, String::new());
        if let Some(i) = rest.find("((\"") {
            let r = &rest[i + 3..];
            if let Some(j) = r.find('"') {
                process = r[..j].to_string();
            }
        }
        if let Some(i) = rest.find("pid=") {
            pid = rest[i + 4..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
        }
        out.push(Conn { proto, local: f[4].into(), remote: f[5].into(), state: norm_state(f[1]), pid, process });
    }
    out
}

/// `netstat -tunap` (Linux fallback): `tcp  0  0 0.0.0.0:22  0.0.0.0:*  LISTEN  812/sshd` (UDP often has no state).
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_netstat_linux(text: &str) -> Vec<Conn> {
    let mut out = vec![];
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 {
            continue;
        }
        let proto = f[0].to_ascii_lowercase();
        if !(proto.starts_with("tcp") || proto.starts_with("udp")) {
            continue;
        }
        let has_state = !(f[5] == "-" || f[5].contains('/'));
        let (state, prog) = if has_state { (f[5], f[6..].join(" ")) } else { ("", f[5..].join(" ")) };
        let (pid, process) = match prog.split_once('/') {
            Some((p, n)) => (p.parse().unwrap_or(0), n.to_string()),
            None => (0, String::new()),
        };
        out.push(Conn { proto, local: f[3].into(), remote: f[4].into(), state: norm_state(state), pid, process });
    }
    out
}

/// Fill in process names from the sampler's snapshot and put the interesting rows first.
pub fn finish_conns(mut v: Vec<Conn>, snap: &Snap) -> Vec<Conn> {
    let names: HashMap<u32, &str> = snap.procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
    for c in &mut v {
        if c.process.is_empty() {
            c.process = match c.pid {
                0 => String::new(),
                4 if cfg!(windows) => "System".into(),
                pid => names.get(&pid).map(|s| s.to_string()).unwrap_or_default(),
            };
        }
    }
    let rank = |s: &str| match s {
        "established" => 0,
        "listening" => 1,
        "" => 2,
        _ => 3,
    };
    v.sort_by(|a, b| rank(&a.state).cmp(&rank(&b.state)).then_with(|| a.process.to_lowercase().cmp(&b.process.to_lowercase())).then_with(|| a.local.cmp(&b.local)));
    v
}

/// `systemctl list-units --type=service --all --plain --no-legend --full`:
/// `ssh.service  loaded  active  running  OpenBSD Secure Shell server` (a leading ● marks broken units).
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_systemctl_units(text: &str) -> Vec<Service> {
    let mut out = vec![];
    for line in text.lines() {
        let mut f: Vec<&str> = line.split_whitespace().collect();
        if matches!(f.first(), Some(&"●") | Some(&"*")) {
            f.remove(0);
        }
        if f.len() < 4 || !f[0].contains('.') {
            continue;
        }
        let state = match (f[2], f[3]) {
            (_, "running") => "running",
            ("failed", _) => "failed",
            ("active", "exited") => "exited",
            ("activating", _) => "starting",
            ("deactivating", _) => "stopping",
            ("inactive", _) => "stopped",
            (_, s) => s,
        };
        out.push(Service {
            name: f[0].trim_end_matches(".service").to_string(),
            display: f[4..].join(" "),
            state: state.to_string(),
            start: String::new(),
            pid: 0,
            desc: String::new(),
        });
    }
    out
}

/// `systemctl list-unit-files --no-legend`: `ssh.service  enabled  enabled` -> name -> state.
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_unit_files(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (n, s) = (f.next()?, f.next()?);
            n.contains('.').then(|| (n.to_string(), s.to_string()))
        })
        .collect()
}

/// A `.desktop` autostart entry -> (name, command, enabled). `Hidden=true` means removed, and
/// `X-GNOME-Autostart-enabled=false` means switched off.
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_desktop(text: &str) -> Option<(String, String, bool)> {
    let (mut name, mut exec, mut on, mut in_entry) = (None, String::new(), true, false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "Name" if name.is_none() => name = Some(v.trim().to_string()),
            "Exec" => exec = v.trim().to_string(),
            "Hidden" if v.trim().eq_ignore_ascii_case("true") => on = false,
            "X-GNOME-Autostart-enabled" if v.trim().eq_ignore_ascii_case("false") => on = false,
            _ => {}
        }
    }
    Some((name?, exec, on))
}

/// `lspci` lines for display adapters: `01:00.0 VGA compatible controller: NVIDIA Corporation TU102 [GeForce RTX 2080 Ti] (rev a1)`.
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_lspci(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.contains("VGA compatible controller") || l.contains("3D controller") || l.contains("Display controller"))
        .filter_map(|l| {
            let s = l.split_once("controller: ")?.1;
            let s = s.split(" (rev ").next().unwrap_or(s);
            Some(s.trim().to_string())
        })
        .collect()
}

/// `SCSI\Disk&Ven_NVMe&Prod_Samsung_SSD_960\5&87ca3f&0&000000` -> "NVMe Samsung SSD 960".
#[cfg_attr(not(windows), allow(dead_code))]
pub fn disk_model(dev: &str) -> String {
    let part = dev.split('\\').nth(1).unwrap_or(dev);
    let mut words = vec![];
    for kv in part.split('&') {
        if let Some(v) = kv.strip_prefix("Ven_").or_else(|| kv.strip_prefix("Prod_")) {
            words.push(v.replace('_', " "));
        }
    }
    if words.is_empty() { part.replace('_', " ") } else { words.join(" ").trim().to_string() }
}

// ------------------------------------------------------------------ startup apps

#[cfg(windows)]
fn startup() -> Vec<StartupItem> {
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
    // StartupApproved flags (subkey -> value name -> on/off), both hives in two calls
    let mut approved: HashMap<(String, String), bool> = HashMap::new();
    for hive in ["HKCU", "HKLM"] {
        if let Some(t) = run("reg", &["query", &format!(r"{hive}\{APPROVED}"), "/s"]) {
            for (key, name, _, data) in parse_reg(&t) {
                let sub = key.rsplit('\\').next().unwrap_or("").to_string();
                if let Some(on) = approved_on(&data) {
                    approved.insert((format!("{hive}\\{sub}"), name.to_lowercase()), on);
                }
            }
        }
    }
    let mut out = vec![];
    for (key, loc, flag) in [
        (format!(r"HKCU\{RUN}"), "HKCU Run", r"HKCU\Run"),
        (format!(r"HKLM\{RUN}"), "HKLM Run", r"HKLM\Run"),
        (r"HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run".to_string(), "HKLM Run (32-bit)", r"HKLM\Run32"),
    ] {
        let Some(t) = run("reg", &["query", &key]) else { continue };
        for (k, name, _, data) in parse_reg(&t) {
            if !k.eq_ignore_ascii_case(&expand_hive(&key)) {
                continue; // only the Run key itself, not subkeys
            }
            let enabled = Some(approved.get(&(flag.to_string(), name.to_lowercase())).copied().unwrap_or(true));
            out.push(StartupItem { name, command: data.trim().to_string(), location: loc.into(), enabled });
        }
    }
    let folders = [
        (std::env::var_os("APPDATA").map(|a| std::path::PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs\Startup")), "startup folder", r"HKCU\StartupFolder"),
        (std::env::var_os("ProgramData").map(|a| std::path::PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs\StartUp")), "startup folder (all users)", r"HKLM\StartupFolder"),
    ];
    for (dir, loc, flag) in folders {
        let Some(dir) = dir else { continue };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            if file.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            let name = file.strip_suffix(".lnk").unwrap_or(&file).to_string();
            let enabled = Some(approved.get(&(flag.to_string(), file.to_lowercase())).copied().unwrap_or(true));
            out.push(StartupItem { name, command: e.path().display().to_string(), location: loc.into(), enabled });
        }
    }
    out
}

#[cfg(windows)]
fn expand_hive(k: &str) -> String {
    k.replacen("HKCU\\", "HKEY_CURRENT_USER\\", 1).replacen("HKLM\\", "HKEY_LOCAL_MACHINE\\", 1)
}

#[cfg(not(windows))]
fn startup() -> Vec<StartupItem> {
    let mut out = vec![];
    let config = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).filter(|p| p.is_absolute()).or_else(|| dirs::home_dir().map(|h| h.join(".config")));
    // a user entry with the same file name overrides the system one
    let mut seen = std::collections::HashSet::new();
    let mut dirs_: Vec<(std::path::PathBuf, &str)> = vec![];
    if let Some(c) = config {
        dirs_.push((c.join("autostart"), "~/.config/autostart"));
    }
    dirs_.push(("/etc/xdg/autostart".into(), "/etc/xdg/autostart"));
    for (dir, loc) in dirs_ {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<_> = rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "desktop")).collect();
        files.sort_by_key(|e| e.file_name());
        for e in files {
            if !seen.insert(e.file_name()) {
                continue;
            }
            let Ok(txt) = std::fs::read_to_string(e.path()) else { continue };
            if let Some((name, exec, on)) = parse_desktop(&txt) {
                out.push(StartupItem { name, command: exec, location: loc.into(), enabled: Some(on) });
            }
        }
    }
    if let Some(t) = run("systemctl", &["--user", "list-unit-files", "--state=enabled", "--no-legend", "--no-pager", "--plain"]) {
        for (unit, _) in parse_unit_files(&t) {
            out.push(StartupItem { name: unit, command: String::new(), location: "systemd --user".into(), enabled: Some(true) });
        }
    }
    out
}

// ------------------------------------------------------------------ services

/// Windows: the service control manager directly (advapi32, what `sc queryex` uses), so descriptions come back
/// resolved. Start type + description are cached from the previous list (they rarely change).
#[cfg(windows)]
fn services(prev: Option<&[Service]>) -> Vec<Service> {
    let cache: HashMap<&str, (&str, &str)> = prev.unwrap_or(&[]).iter().map(|s| (s.name.as_str(), (s.start.as_str(), s.desc.as_str()))).collect();
    let mut v = scm::list(&cache);
    v.sort_by_cached_key(|s| s.name.to_lowercase());
    v
}

#[cfg(not(windows))]
fn services(_prev: Option<&[Service]>) -> Vec<Service> {
    let Some(t) = run("systemctl", &["list-units", "--type=service", "--all", "--no-pager", "--plain", "--no-legend", "--full"]) else { return vec![] };
    let mut v = parse_systemctl_units(&t);
    if let Some(t) = run("systemctl", &["list-unit-files", "--type=service", "--no-pager", "--no-legend", "--plain"]) {
        let files: HashMap<String, String> = parse_unit_files(&t).into_iter().map(|(n, s)| (n.trim_end_matches(".service").to_string(), s)).collect();
        for s in &mut v {
            // instances (getty@tty1) inherit their template's state (getty@)
            let tmpl = s.name.split_once('@').map(|(a, _)| format!("{a}@"));
            if let Some(st) = files.get(&s.name).or_else(|| tmpl.and_then(|t| files.get(&t))) {
                s.start = st.clone();
            }
        }
    }
    v.sort_by_cached_key(|s| s.name.to_lowercase());
    v
}

#[cfg(windows)]
mod scm {
    use super::Service;
    use std::collections::HashMap;

    #[repr(C)]
    struct Status {
        _service_type: u32,
        state: u32,
        _controls: u32,
        _exit: u32,
        _svc_exit: u32,
        _checkpoint: u32,
        _wait: u32,
        pid: u32,
        _flags: u32,
    }
    #[repr(C)]
    struct Entry {
        name: *const u16,
        display: *const u16,
        status: Status,
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenSCManagerW(machine: *const u16, db: *const u16, access: u32) -> isize;
        fn EnumServicesStatusExW(h: isize, level: u32, kind: u32, state: u32, buf: *mut u8, size: u32, needed: *mut u32, count: *mut u32, resume: *mut u32, group: *const u16) -> i32;
        fn OpenServiceW(h: isize, name: *const u16, access: u32) -> isize;
        fn QueryServiceConfigW(h: isize, buf: *mut u8, size: u32, needed: *mut u32) -> i32;
        fn QueryServiceConfig2W(h: isize, level: u32, buf: *mut u8, size: u32, needed: *mut u32) -> i32;
        fn CloseServiceHandle(h: isize) -> i32;
    }
    const ERROR_MORE_DATA: i32 = 234;

    fn wstr(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        // SAFETY: SCM strings are NUL-terminated and live inside the buffer we own.
        unsafe {
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// One QueryServiceConfig*W call into an 8-byte aligned buffer, growing it once if needed.
    fn query(buf: &mut Vec<u64>, f: impl Fn(*mut u8, u32, *mut u32) -> i32) -> bool {
        let mut need = 0u32;
        if f(buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut need) != 0 {
            return true;
        }
        if need as usize > buf.len() * 8 && need < (1 << 20) {
            buf.resize(need as usize / 8 + 1, 0);
            return f(buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut need) != 0;
        }
        false
    }

    pub fn list(cache: &HashMap<&str, (&str, &str)>) -> Vec<Service> {
        let mut out = vec![];
        let scm = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), 0x0001 | 0x0004) }; // CONNECT | ENUMERATE_SERVICE
        if scm == 0 {
            return out;
        }
        let mut buf: Vec<u64> = vec![0; (256 << 10) / 8];
        let mut resume = 0u32;
        let mut cfg: Vec<u64> = vec![0; 8192 / 8];
        loop {
            let (mut need, mut count) = (0u32, 0u32);
            // SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL
            let ok = unsafe { EnumServicesStatusExW(scm, 0, 0x30, 3, buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut need, &mut count, &mut resume, std::ptr::null()) };
            let more = ok == 0 && std::io::Error::last_os_error().raw_os_error() == Some(ERROR_MORE_DATA);
            if ok == 0 && !more {
                break;
            }
            let entries = buf.as_ptr() as *const Entry;
            for i in 0..count as usize {
                // SAFETY: the SCM wrote `count` ENUM_SERVICE_STATUS_PROCESSW records at the start of `buf`.
                let e = unsafe { &*entries.add(i) };
                let name = wstr(e.name);
                let state = match e.status.state {
                    1 => "stopped",
                    2 => "starting",
                    3 => "stopping",
                    4 => "running",
                    5 => "resuming",
                    6 => "pausing",
                    7 => "paused",
                    _ => "unknown",
                };
                let (start, desc) = match cache.get(name.as_str()) {
                    Some(&(s, d)) => (s.to_string(), d.to_string()),
                    None => config(scm, &name, &mut cfg),
                };
                out.push(Service { display: wstr(e.display), name, state: state.into(), start, pid: e.status.pid, desc });
            }
            if !more {
                break;
            }
            if need as usize > buf.len() * 8 {
                buf.resize(need as usize / 8 + 1, 0);
            }
        }
        unsafe { CloseServiceHandle(scm) };
        out
    }

    /// (start type, description) for one service.
    fn config(scm: isize, name: &str, buf: &mut Vec<u64>) -> (String, String) {
        let h = unsafe { OpenServiceW(scm, wide(name).as_ptr(), 0x0001) }; // SERVICE_QUERY_CONFIG
        if h == 0 {
            return (String::new(), String::new());
        }
        let mut start = String::new();
        // QUERY_SERVICE_CONFIGW: dwServiceType, dwStartType, ...
        if query(buf, |p, n, need| unsafe { QueryServiceConfigW(h, p, n, need) }) {
            start = match buf[0] >> 32 {
                0 => "boot",
                1 => "system",
                2 => "auto",
                3 => "manual",
                4 => "disabled",
                _ => "",
            }
            .to_string();
            // SERVICE_CONFIG_DELAYED_AUTO_START_INFO: BOOL fDelayedAutostart
            if start == "auto" && query(buf, |p, n, need| unsafe { QueryServiceConfig2W(h, 3, p, n, need) }) && buf[0] as u32 != 0 {
                start = "auto (delayed)".into();
            }
        }
        let mut desc = String::new();
        // SERVICE_CONFIG_DESCRIPTION: SERVICE_DESCRIPTIONW { LPWSTR lpDescription }
        if query(buf, |p, n, need| unsafe { QueryServiceConfig2W(h, 1, p, n, need) }) {
            desc = wstr(buf[0] as usize as *const u16);
        }
        unsafe { CloseServiceHandle(h) };
        (start, desc)
    }
}

// ------------------------------------------------------------------ connections

fn connections(snap: &Snap) -> Vec<Conn> {
    #[cfg(windows)]
    let v = run("netstat", &["-ano"]).map(|t| parse_netstat_win(&t)).unwrap_or_default();
    #[cfg(not(windows))]
    let v = match run("ss", &["-tunap"]) {
        Some(t) => parse_ss(&t),
        None => run("netstat", &["-tunap"]).map(|t| parse_netstat_linux(&t)).unwrap_or_default(),
    };
    finish_conns(v, snap)
}

// ------------------------------------------------------------------ system info

fn info() -> Info {
    use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};
    let row = |k: &str, v: String| (k.to_string(), v);
    let mut sections = vec![];

    let mut os = vec![];
    os.push(row("system", System::long_os_version().or_else(System::name).unwrap_or_default()));
    if let Some(v) = System::os_version() {
        os.push(row("version", v));
    }
    if cfg!(windows) {
        os.push(row("build", System::kernel_version().unwrap_or_default()));
    } else {
        os.push(row("kernel", System::kernel_long_version()));
    }
    os.push(row("hostname", System::host_name().unwrap_or_default()));
    os.push(row("arch", System::cpu_arch()));
    sections.push(Section { icon: "window", title: "os".into(), rows: os });

    let sys = System::new_with_specifics(RefreshKind::nothing().with_cpu(CpuRefreshKind::everything()).with_memory(MemoryRefreshKind::everything()));
    let mut cpu = vec![];
    if let Some(c) = sys.cpus().first() {
        cpu.push(row("model", c.brand().trim().to_string()));
        if !c.vendor_id().is_empty() {
            cpu.push(row("vendor", c.vendor_id().to_string()));
        }
    }
    let threads = sys.cpus().len();
    cpu.push(row("cores", match System::physical_core_count() {
        Some(p) => format!("{p} cores · {threads} threads"),
        None => format!("{threads} threads"),
    }));
    #[cfg(windows)]
    if let Some(t) = run("reg", &["query", r"HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0", "/v", "~MHz"]) {
        if let Some((_, _, _, v)) = parse_reg(&t).into_iter().next() {
            if let Ok(mhz) = u64::from_str_radix(v.trim().trim_start_matches("0x"), 16) {
                cpu.push(row("base clock", format!("{:.2} GHz", mhz as f64 / 1000.0)));
            }
        }
    }
    #[cfg(not(windows))]
    if let Ok(s) = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq") {
        if let Ok(khz) = s.trim().parse::<u64>() {
            cpu.push(row("max clock", format!("{:.2} GHz", khz as f64 / 1e6)));
        }
    }
    sections.push(Section { icon: "system", title: "cpu".into(), rows: cpu });

    let gib = |b: u64| format!("{:.1} GB", b as f64 / (1u64 << 30) as f64);
    let mut mem = vec![row("installed", gib(sys.total_memory()))];
    if sys.total_swap() > 0 {
        mem.push(row(if cfg!(windows) { "page file" } else { "swap" }, gib(sys.total_swap())));
    }
    sections.push(Section { icon: "chart", title: "memory".into(), rows: mem });

    let gpus = gpus();
    if !gpus.is_empty() {
        sections.push(Section { icon: "gauge", title: "gpu".into(), rows: gpus });
    }
    let board = board();
    if !board.is_empty() {
        sections.push(Section { icon: "package", title: "board".into(), rows: board });
    }

    let mut disks = drive_models();
    let list = Disks::new_with_refreshed_list();
    let mut vols: Vec<_> = list.list().iter().filter(|d| d.total_space() > 0).collect();
    vols.sort_by_key(|d| d.mount_point().to_path_buf());
    let mut seen = std::collections::HashSet::new();
    for d in vols {
        if !seen.insert(d.name().to_os_string()) && !d.name().is_empty() {
            continue;
        }
        let kind = match d.kind() {
            sysinfo::DiskKind::SSD => " · ssd",
            sysinfo::DiskKind::HDD => " · hdd",
            _ => "",
        };
        let label = d.mount_point().to_string_lossy().trim_end_matches('\\').to_string();
        disks.push((label, format!("{} · {}{kind} · {} free", gib(d.total_space()), d.file_system().to_string_lossy(), gib(d.available_space()))));
    }
    if !disks.is_empty() {
        sections.push(Section { icon: "storage", title: "disks".into(), rows: disks });
    }

    // network adapters that have an address (loopback and link-local-only ones left out)
    let nets = sysinfo::Networks::new_with_refreshed_list();
    let mut adapters: Vec<(String, String)> = nets
        .list()
        .iter()
        .filter_map(|(name, n)| {
            let ips: Vec<String> = n
                .ip_networks()
                .iter()
                .filter(|ip| !ip.addr.is_loopback() && !matches!(ip.addr, std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80))
                .map(|ip| format!("{}/{}", ip.addr, ip.prefix))
                .collect();
            if ips.is_empty() {
                return None;
            }
            let mac = n.mac_address().to_string();
            let mac = if mac == "00:00:00:00:00:00" { String::new() } else { format!(" · {mac}") };
            Some((name.clone(), format!("{}{mac}", ips.join(", "))))
        })
        .collect();
    adapters.sort();
    if !adapters.is_empty() {
        sections.push(Section { icon: "cloud", title: "network".into(), rows: adapters });
    }
    Info { sections, boot: System::boot_time() }
}

/// GPUs: nvidia-smi when it's there (name, VRAM, driver), plus the OS's own list of display adapters.
fn gpus() -> Vec<(String, String)> {
    let mut rows = vec![];
    let mut named = vec![];
    if let Some(t) = run("nvidia-smi", &["--query-gpu=name,memory.total,driver_version", "--format=csv,noheader,nounits"]) {
        for l in t.lines() {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() >= 3 {
                rows.push((f[0].to_string(), format!("{:.0} GB VRAM · driver {}", f[1].parse::<f64>().unwrap_or(0.0) / 1024.0, f[2])));
                named.push(f[0].to_lowercase());
            }
        }
    }
    #[cfg(windows)]
    {
        const CLASS: &str = r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
        if let Some(t) = run("reg", &["query", CLASS, "/s", "/v", "DriverDesc"]) {
            for (key, _, _, name) in parse_reg(&t) {
                if named.contains(&name.to_lowercase()) {
                    continue;
                }
                let short = key.replacen("HKEY_LOCAL_MACHINE", "HKLM", 1);
                let mut detail = String::new();
                if let Some(t) = run("reg", &["query", &short, "/v", "HardwareInformation.qwMemorySize"]) {
                    if let Some((_, _, _, v)) = parse_reg(&t).into_iter().next() {
                        if let Ok(b) = u64::from_str_radix(v.trim().trim_start_matches("0x"), 16) {
                            detail = format!("{:.0} GB VRAM", b as f64 / (1u64 << 30) as f64);
                        }
                    }
                }
                if let Some(t) = run("reg", &["query", &short, "/v", "DriverVersion"]) {
                    if let Some((_, _, _, v)) = parse_reg(&t).into_iter().next() {
                        detail = if detail.is_empty() { format!("driver {v}") } else { format!("{detail} · driver {v}") };
                    }
                }
                rows.push((name, detail));
            }
        }
    }
    #[cfg(not(windows))]
    if let Some(t) = run("lspci", &[]) {
        for name in parse_lspci(&t) {
            if !named.is_empty() && name.to_lowercase().contains("nvidia") {
                continue; // nvidia-smi already described it
            }
            rows.push((name, String::new()));
        }
    }
    rows
}

fn board() -> Vec<(String, String)> {
    let mut rows = vec![];
    let junk = |s: &str| s.is_empty() || s.eq_ignore_ascii_case("default string") || s.eq_ignore_ascii_case("to be filled by o.e.m.") || s.eq_ignore_ascii_case("system product name");
    #[cfg(windows)]
    {
        let Some(t) = run("reg", &["query", r"HKLM\HARDWARE\DESCRIPTION\System\BIOS"]) else { return rows };
        let v: HashMap<String, String> = parse_reg(&t).into_iter().map(|(_, n, _, d)| (n, d.trim().to_string())).collect();
        let get = |k: &str| v.get(k).cloned().unwrap_or_default();
        let pair = |a: String, b: String| if junk(&b) { a } else if junk(&a) { b } else { format!("{a} {b}") };
        let sys = pair(get("SystemManufacturer"), get("SystemProductName"));
        let mb = pair(get("BaseBoardManufacturer"), get("BaseBoardProduct"));
        if !junk(&mb) {
            rows.push(("motherboard".to_string(), mb.clone()));
        }
        if !junk(&sys) && sys != mb {
            rows.push(("system".to_string(), sys));
        }
        let bios = pair(get("BIOSVendor"), get("BIOSVersion"));
        if !junk(&bios) {
            let date = get("BIOSReleaseDate");
            rows.push(("bios".to_string(), if date.is_empty() { bios } else { format!("{bios} · {date}") }));
        }
    }
    #[cfg(not(windows))]
    {
        let rd = |f: &str| std::fs::read_to_string(format!("/sys/class/dmi/id/{f}")).map(|s| s.trim().to_string()).unwrap_or_default();
        let pair = |a: String, b: String| if junk(&b) { a } else if junk(&a) { b } else { format!("{a} {b}") };
        let mb = pair(rd("board_vendor"), rd("board_name"));
        let sys = pair(rd("sys_vendor"), rd("product_name"));
        if !junk(&mb) {
            rows.push(("motherboard".to_string(), mb.clone()));
        }
        if !junk(&sys) && sys != mb {
            rows.push(("system".to_string(), sys));
        }
        let bios = pair(rd("bios_vendor"), rd("bios_version"));
        if !junk(&bios) {
            let date = rd("bios_date");
            rows.push(("bios".to_string(), if date.is_empty() { bios } else { format!("{bios} · {date}") }));
        }
    }
    rows
}

/// Physical drive models.
fn drive_models() -> Vec<(String, String)> {
    let mut rows = vec![];
    #[cfg(windows)]
    if let Some(t) = run("reg", &["query", r"HKLM\SYSTEM\CurrentControlSet\Services\disk\Enum"]) {
        for (_, name, ty, data) in parse_reg(&t) {
            if ty == "REG_SZ" && name.chars().all(|c| c.is_ascii_digit()) {
                rows.push((format!("drive {name}"), disk_model(&data)));
            }
        }
    }
    #[cfg(not(windows))]
    if let Ok(rd) = std::fs::read_dir("/sys/block") {
        let mut devs: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        devs.sort();
        for d in devs {
            if ["loop", "ram", "zram", "dm-", "sr", "fd", "md"].iter().any(|p| d.starts_with(p)) {
                continue;
            }
            let base = format!("/sys/block/{d}");
            let model = std::fs::read_to_string(format!("{base}/device/model")).map(|s| s.trim().to_string()).unwrap_or_default();
            let size = std::fs::read_to_string(format!("{base}/size")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) * 512;
            if size == 0 {
                continue;
            }
            let rot = std::fs::read_to_string(format!("{base}/queue/rotational")).map(|s| if s.trim() == "1" { " · hdd" } else { " · ssd" }).unwrap_or("");
            let model = if model.is_empty() { "disk".to_string() } else { model };
            rows.push((d, format!("{model} · {:.0} GB{rot}", size as f64 / 1e9)));
        }
    }
    rows
}
