//! The system app's background sampling. Everything slow happens here, off the UI thread: sysinfo refreshes
//! (CPU, memory, disks, network, ~700 processes) and `nvidia-smi` on its own thread.
//! Each second the sampler publishes an immutable `Snap` behind an `Arc` and wakes the UI, which only swaps the
//! pointer and draws. Sampling pauses by itself when the UI stops polling (pane hidden) and ends on drop.

use crate::pane::Waker;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{
    CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, Networks, Pid, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, Uid, UpdateKind, Users,
};

/// Seconds of history the sparklines keep.
pub const HISTORY: usize = 120;

// ------------------------------------------------------------------ data the UI draws

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    /// % of the whole machine (Task Manager style), not of one core.
    pub cpu: f32,
    pub mem: u64,
    pub threads: u32,
    pub user: Arc<str>,
    /// Parent pid (0 = none). Only trust it when the parent started before the child (pids get reused).
    pub ppid: u32,
    /// Start time in platform units (FILETIME on Windows, seconds on Unix): only compared with each other.
    pub start: u64,
    /// I/O bytes per second (Windows: all I/O, as Process Explorer counts it; Linux: /proc/<pid>/io).
    pub read: f64,
    pub write: f64,
}

#[derive(Clone, Debug)]
pub struct DiskRow {
    pub label: String,
    pub total: u64,
    pub free: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Snap {
    pub seq: u64,
    pub cpu_hist: VecDeque<f32>,
    pub cores: Vec<f32>,
    /// Per logical CPU usage history (same length as `cores`).
    pub core_hist: Vec<VecDeque<f32>>,
    /// Per logical CPU frequency in MHz (0 = unknown).
    pub core_mhz: Vec<u64>,
    pub ghz: f64,
    pub ram_used: u64,
    pub ram_total: u64,
    pub ram_hist: VecDeque<f32>,
    pub disks: Vec<DiskRow>,
    pub net_down: VecDeque<f64>,
    pub net_up: VecDeque<f64>,
    pub disk_r: VecDeque<f64>,
    pub disk_w: VecDeque<f64>,
    pub procs: Vec<Proc>,
    /// Sum of every process's threads.
    pub threads: u64,
    /// How long this sample took on the sampler thread.
    pub sample_ms: f64,
}

#[derive(Clone, Debug)]
pub struct Gpu {
    pub name: String,
    pub util: f32,
    pub used: f64,
    pub total: f64,
    pub temp: f32,
    pub power: f32,
    pub limit: f32,
    pub fan: f32,
}

#[derive(Clone, Debug)]
pub enum GpuState {
    Pending,
    /// No nvidia-smi on this machine: the card is hidden.
    Missing,
    /// nvidia-smi exists but reported nothing usable.
    Failed,
    Ok(Gpu),
}

pub struct Inner {
    pub snap: Option<Arc<Snap>>,
    pub gpu: GpuState,
    pub gpu_hist: VecDeque<f32>,
    /// Results of background actions (kill) for the UI to show as notices.
    pub msgs: Vec<String>,
}

pub struct Shared {
    pub inner: Mutex<Inner>,
    /// Milliseconds since `epoch` when the UI last polled. The sampler idles when this goes stale.
    last_poll: AtomicU64,
    epoch: Instant,
    stop: AtomicBool,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            inner: Mutex::new(Inner { snap: None, gpu: GpuState::Pending, gpu_hist: VecDeque::new(), msgs: vec![] }),
            last_poll: AtomicU64::new(0),
            epoch: Instant::now(),
            stop: AtomicBool::new(false),
        })
    }
    pub fn touch(&self) {
        self.last_poll.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
    fn active(&self) -> bool {
        let now = self.epoch.elapsed().as_millis() as u64;
        now.saturating_sub(self.last_poll.load(Ordering::Relaxed)) < 2500
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Start the sampler and GPU threads. They run until `Shared::stop`.
pub fn start(shared: Arc<Shared>, waker: Waker) {
    shared.touch();
    {
        let (s, w) = (shared.clone(), waker.clone());
        std::thread::Builder::new().name("oriel-sys".into()).spawn(move || sampler_loop(s, w)).ok();
    }
    std::thread::Builder::new().name("oriel-gpu".into()).spawn(move || gpu_loop(shared, waker)).ok();
}

fn push<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() >= HISTORY {
        q.pop_front();
    }
    q.push_back(v);
}

// ------------------------------------------------------------------ the sampler

pub struct Sampler {
    sys: sysinfo::System,
    disks: Disks,
    nets: Networks,
    users: HashMap<Uid, Arc<str>>,
    user_of: HashMap<u32, Arc<str>>,
    #[cfg(windows)]
    last_prune: Instant,
    ncpu: f32,
    hist: Snap,
    last: Instant,
    last_net: Option<(u64, u64)>,
    last_io: Option<(u64, u64)>,
    last_freq: Option<Instant>,
    last_disk_list: Instant,
    /// pid -> (cpu time, bytes read, bytes written) at the previous sample.
    #[cfg(windows)]
    prev_cpu: HashMap<u32, (u64, u64, u64)>,
    empty: Arc<str>,
}

impl Sampler {
    pub fn new() -> Sampler {
        let sys = sysinfo::System::new_with_specifics(
            RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_cpu_usage().with_frequency()).with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        let ncpu = sys.cpus().len().max(1) as f32;
        let users = Users::new_with_refreshed_list().list().iter().map(|u| (u.id().clone(), Arc::from(u.name()))).collect();
        Sampler {
            sys,
            disks: Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing().with_storage().with_io_usage()),
            nets: Networks::new_with_refreshed_list(),
            users,
            user_of: HashMap::new(),
            #[cfg(windows)]
            last_prune: Instant::now(),
            ncpu,
            hist: Snap::default(),
            last: Instant::now(),
            last_net: None,
            last_io: None,
            last_freq: None,
            last_disk_list: Instant::now(),
            #[cfg(windows)]
            prev_cpu: HashMap::new(),
            empty: Arc::from(""),
        }
    }

    /// Take one sample and return the snapshot the UI will draw.
    pub fn sample(&mut self) -> Snap {
        let t0 = Instant::now();
        let dt = self.last.elapsed().as_secs_f64().max(0.2);
        self.last = Instant::now();

        // cpu + memory
        self.sys.refresh_cpu_usage();
        if self.last_freq.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            self.sys.refresh_cpu_frequency();
            self.last_freq = Some(Instant::now());
        }
        self.sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let cores: Vec<f32> = self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        if self.hist.core_hist.len() != cores.len() {
            self.hist.core_hist = vec![VecDeque::with_capacity(HISTORY); cores.len()];
        }
        for (q, &v) in self.hist.core_hist.iter_mut().zip(&cores) {
            push(q, v);
        }
        let core_mhz: Vec<u64> = self.sys.cpus().iter().map(|c| c.frequency()).collect();
        let total = if cores.is_empty() { 0.0 } else { cores.iter().sum::<f32>() / cores.len() as f32 };
        push(&mut self.hist.cpu_hist, total);
        let freqs: Vec<u64> = core_mhz.iter().copied().filter(|&f| f > 0).collect();
        let ghz = if freqs.is_empty() { 0.0 } else { freqs.iter().sum::<u64>() as f64 / freqs.len() as f64 / 1000.0 };
        let (ram_used, ram_total) = (self.sys.used_memory(), self.sys.total_memory());
        push(&mut self.hist.ram_hist, if ram_total > 0 { 100.0 * ram_used as f32 / ram_total as f32 } else { 0.0 });

        // disks: space per drive/mount, io summed per device (btrfs subvolumes share one device, don't count twice)
        if self.last_disk_list.elapsed() > Duration::from_secs(15) {
            self.disks.refresh_specifics(true, DiskRefreshKind::nothing().with_storage().with_io_usage());
            self.last_disk_list = Instant::now();
        } else {
            for d in self.disks.list_mut() {
                d.refresh_specifics(DiskRefreshKind::nothing().with_storage().with_io_usage());
            }
        }
        let mut seen = std::collections::HashSet::new();
        let (mut rd, mut wr) = (0u64, 0u64);
        let mut disks = vec![];
        let mut list: Vec<&sysinfo::Disk> = self.disks.list().iter().collect();
        // shortest mount point first, so "/" wins over "/home" for the same btrfs device
        list.sort_by_key(|d| d.mount_point().as_os_str().len());
        for d in list {
            if d.total_space() == 0 {
                continue;
            }
            let key = d.name().to_os_string();
            let dev_key = if key.is_empty() { d.mount_point().as_os_str().to_os_string() } else { key };
            if !seen.insert(dev_key) {
                continue;
            }
            let u = d.usage();
            rd += u.total_read_bytes;
            wr += u.total_written_bytes;
            disks.push(DiskRow { label: disk_label(d.mount_point()), total: d.total_space(), free: d.available_space() });
        }
        disks.sort_by(|a, b| a.label.cmp(&b.label));
        let (r, w) = rate(&mut self.last_io, (rd, wr), dt);
        push(&mut self.hist.disk_r, r);
        push(&mut self.hist.disk_w, w);

        // network (loopback left out)
        self.nets.refresh(true);
        let (mut down, mut up) = (0u64, 0u64);
        for (name, n) in self.nets.list() {
            let l = name.to_ascii_lowercase();
            if l == "lo" || l.starts_with("loopback") {
                continue;
            }
            down += n.total_received();
            up += n.total_transmitted();
        }
        let (d, u) = rate(&mut self.last_net, (down, up), dt);
        push(&mut self.hist.net_down, d);
        push(&mut self.hist.net_up, u);

        let procs = self.processes(dt);
        if self.user_of.len() > procs.len() + 256 {
            let live: std::collections::HashSet<u32> = procs.iter().map(|p| p.pid).collect();
            self.user_of.retain(|k, _| live.contains(k));
        }

        let mut s = self.hist.clone();
        s.seq = self.hist.seq + 1;
        self.hist.seq = s.seq;
        s.cores = cores;
        s.core_mhz = core_mhz;
        s.ghz = ghz;
        s.ram_used = ram_used;
        s.ram_total = ram_total;
        s.disks = disks;
        s.threads = procs.iter().map(|p| p.threads as u64).sum();
        s.procs = procs;
        s.sample_ms = t0.elapsed().as_secs_f64() * 1000.0;
        s
    }

    /// Every process. Windows: one NtQuerySystemInformation call gives name, parent, threads, working set, CPU
    /// time and I/O counters for all of them (~5 ms); sysinfo's own per-process refresh opens every process and
    /// cost ~80 ms here. sysinfo is only asked for the user of processes we haven't seen before.
    #[cfg(windows)]
    fn processes(&mut self, dt: f64) -> Vec<Proc> {
        let raw = nt::snapshot();
        let new: Vec<Pid> = raw.iter().filter(|r| r.pid != 0 && !self.user_of.contains_key(&r.pid)).map(|r| Pid::from_u32(r.pid)).collect();
        if !new.is_empty() {
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&new),
                true,
                ProcessRefreshKind::nothing().with_user(UpdateKind::OnlyIfNotSet).without_tasks(),
            );
            let session0: std::collections::HashSet<u32> = raw.iter().filter(|r| r.session == 0).map(|r| r.pid).collect();
            let system: Arc<str> = Arc::from("SYSTEM");
            for pid in &new {
                let mut u = self.sys.process(*pid).and_then(|p| p.user_id()).map(|id| self.user_name(id)).unwrap_or_else(|| self.empty.clone());
                // protected service processes won't show us their token; in session 0 that's SYSTEM (Task Manager says so too)
                if u.is_empty() && session0.contains(&pid.as_u32()) {
                    u = system.clone();
                }
                self.user_of.insert(pid.as_u32(), u);
            }
        }
        // now and then, forget exited processes in sysinfo's table too (each refresh call costs a full scan)
        if self.last_prune.elapsed() > Duration::from_secs(30) {
            self.last_prune = Instant::now();
            let live: std::collections::HashSet<u32> = raw.iter().map(|r| r.pid).collect();
            let dead: Vec<Pid> = self.sys.processes().keys().filter(|p| !live.contains(&p.as_u32())).copied().collect();
            if !dead.is_empty() {
                self.sys.refresh_processes_specifics(ProcessesToUpdate::Some(&dead), true, ProcessRefreshKind::nothing().without_tasks());
            }
        }
        let scale = 100.0 / (dt * 1e7 * self.ncpu as f64); // cpu times are in 100 ns units
        let mut cpu_now = HashMap::with_capacity(raw.len());
        let mut procs = Vec::with_capacity(raw.len());
        for r in raw {
            cpu_now.insert(r.pid, (r.cpu_time, r.read, r.write));
            if r.pid == 0 {
                continue; // System Idle Process
            }
            let (cpu, read, write) = match self.prev_cpu.get(&r.pid) {
                Some(&(c, rd, wr)) => (r.cpu_time.saturating_sub(c) as f64 * scale, r.read.saturating_sub(rd) as f64 / dt, r.write.saturating_sub(wr) as f64 / dt),
                None => (0.0, 0.0, 0.0),
            };
            procs.push(Proc {
                pid: r.pid,
                cpu: cpu.min(100.0) as f32,
                mem: r.mem,
                threads: r.threads,
                user: self.user_of.get(&r.pid).cloned().unwrap_or_else(|| self.empty.clone()),
                name: r.name,
                ppid: r.ppid,
                start: r.create,
                read,
                write,
            });
        }
        self.prev_cpu = cpu_now;
        procs
    }

    #[cfg(not(windows))]
    fn processes(&mut self, dt: f64) -> Vec<Proc> {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory().with_disk_usage().with_user(UpdateKind::OnlyIfNotSet).without_tasks(),
        );
        let mut procs = Vec::with_capacity(self.sys.processes().len());
        for (pid, p) in self.sys.processes() {
            let pid = pid.as_u32();
            if pid == 0 {
                continue;
            }
            let user = match self.user_of.get(&pid) {
                Some(u) => u.clone(),
                None => {
                    let u = p.user_id().map(|id| self.user_name(id)).unwrap_or_else(|| self.empty.clone());
                    self.user_of.insert(pid, u.clone());
                    u
                }
            };
            // read_bytes / written_bytes are what happened since the previous refresh
            let io = p.disk_usage();
            procs.push(Proc {
                pid,
                name: p.name().to_string_lossy().into_owned(),
                cpu: p.cpu_usage() / self.ncpu,
                mem: p.memory(),
                threads: proc_threads(pid),
                user,
                ppid: p.parent().map(|x| x.as_u32()).unwrap_or(0),
                start: p.start_time(),
                read: io.read_bytes as f64 / dt,
                write: io.written_bytes as f64 / dt,
            });
        }
        procs
    }

    fn user_name(&self, id: &Uid) -> Arc<str> {
        if let Some(n) = self.users.get(id) {
            return n.clone();
        }
        let sid = (**id).to_string();
        let well_known = match sid.as_str() {
            "S-1-5-18" => "SYSTEM",
            "S-1-5-19" => "LOCAL SERVICE",
            "S-1-5-20" => "NETWORK SERVICE",
            "0" => "root",
            _ if sid.starts_with("S-1-5-90-") => "DWM",
            _ if sid.starts_with("S-1-5-96-") => "UMFD",
            _ => "",
        };
        if well_known.is_empty() { self.empty.clone() } else { Arc::from(well_known) }
    }
}

fn rate(last: &mut Option<(u64, u64)>, now: (u64, u64), dt: f64) -> (f64, f64) {
    let out = match *last {
        Some((a, b)) if now.0 >= a && now.1 >= b => ((now.0 - a) as f64 / dt, (now.1 - b) as f64 / dt),
        _ => (0.0, 0.0),
    };
    *last = Some(now);
    out
}

fn disk_label(mount: &Path) -> String {
    let s = mount.to_string_lossy();
    if cfg!(windows) {
        s.trim_end_matches('\\').to_string()
    } else {
        s.into_owned()
    }
}

// ------------------------------------------------------------------ Windows process snapshot
// NtQuerySystemInformation(SystemProcessInformation): every process in one call, what Task Manager does (and what
// nest's procsnap.py did). No extra crate: ntdll is always there.

#[cfg(windows)]
mod nt {
    pub struct Raw {
        pub pid: u32,
        pub name: String,
        pub threads: u32,
        pub mem: u64,
        /// user + kernel time, 100 ns units
        pub cpu_time: u64,
        pub session: u32,
        pub ppid: u32,
        /// creation time, FILETIME
        pub create: u64,
        /// I/O transfer counters: every byte read / written so far
        pub read: u64,
        pub write: u64,
    }

    #[repr(C)]
    struct UnicodeString {
        len: u16,
        _max: u16,
        buf: *const u16,
    }
    // SYSTEM_PROCESS_INFORMATION (64-bit layout), up to the I/O counters; thread records follow it
    #[repr(C)]
    struct Spi {
        next: u32,
        threads: u32,
        _private_ws: i64,
        _hard_faults: u32,
        _threads_hwm: u32,
        _cycle: u64,
        create: i64,
        user: i64,
        kernel: i64,
        name: UnicodeString,
        _prio: i32,
        pid: usize,
        parent: usize,
        _handles: u32,
        session: u32,
        _key: usize,
        _peak_virtual: usize,
        _virtual: usize,
        _page_faults: u32,
        _peak_ws: usize,
        ws: usize,
        _quota_peak_paged: usize,
        _quota_paged: usize,
        _quota_peak_nonpaged: usize,
        _quota_nonpaged: usize,
        _pagefile: usize,
        _peak_pagefile: usize,
        _private_pages: usize,
        _read_ops: u64,
        _write_ops: u64,
        _other_ops: u64,
        read_bytes: u64,
        write_bytes: u64,
        _other_bytes: u64,
    }
    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::size_of::<Spi>() == 256);

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQuerySystemInformation(class: u32, buf: *mut u8, len: u32, ret: *mut u32) -> i32;
    }
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;

    thread_local! {
        // reused between calls (it's ~1-2 MB); u64 storage keeps it 8-byte aligned for the struct reads
        static BUF: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    pub fn snapshot() -> Vec<Raw> {
        BUF.with_borrow_mut(|buf| fill(buf))
    }

    fn fill(buf: &mut Vec<u64>) -> Vec<Raw> {
        let mut out = Vec::with_capacity(800);
        if buf.is_empty() {
            buf.resize((2 << 20) / 8, 0);
        }
        loop {
            let mut need = 0u32;
            let st = unsafe { NtQuerySystemInformation(5, buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut need) };
            if st == STATUS_INFO_LENGTH_MISMATCH {
                let cap = (need as usize + (256 << 10)).max(buf.len() * 8 * 2);
                buf.resize(cap / 8, 0);
                continue;
            }
            if st < 0 {
                return out;
            }
            let base = buf.as_ptr() as *const u8;
            let end = buf.len() * 8;
            let mut off = 0usize;
            while off + std::mem::size_of::<Spi>() <= end {
                // SAFETY: the kernel filled `buf` with a chain of SYSTEM_PROCESS_INFORMATION records whose name
                // buffers point inside `buf`, which outlives this loop.
                let e = unsafe { &*(base.add(off) as *const Spi) };
                let name = if e.name.buf.is_null() || e.name.len == 0 {
                    if e.pid == 0 { "System Idle Process".to_string() } else { "?".to_string() }
                } else {
                    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(e.name.buf, e.name.len as usize / 2) })
                };
                out.push(Raw {
                    pid: e.pid as u32,
                    name,
                    threads: e.threads,
                    mem: e.ws as u64,
                    cpu_time: (e.user + e.kernel).max(0) as u64,
                    session: e.session,
                    ppid: e.parent as u32,
                    create: e.create.max(0) as u64,
                    read: e.read_bytes,
                    write: e.write_bytes,
                });
                if e.next == 0 {
                    break;
                }
                off += e.next as usize;
            }
            return out;
        }
    }
}

/// Linux: field 20 of /proc/<pid>/stat.
#[cfg(target_os = "linux")]
fn proc_threads(pid: u32) -> u32 {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            let rest = &s[s.rfind(')')? + 2..];
            rest.split(' ').nth(17)?.parse().ok()
        })
        .unwrap_or(0)
}
#[cfg(not(any(windows, target_os = "linux")))]
fn proc_threads(_pid: u32) -> u32 {
    0
}

fn sampler_loop(shared: Arc<Shared>, waker: Waker) {
    let mut s = Sampler::new();
    let mut next = Instant::now();
    while !shared.stopped() {
        if !shared.active() {
            std::thread::sleep(Duration::from_millis(200));
            next = Instant::now();
            continue;
        }
        let snap = Arc::new(s.sample());
        shared.lock().snap = Some(snap);
        waker.wake();
        next += Duration::from_secs(1);
        let now = Instant::now();
        if next < now {
            next = now + Duration::from_millis(200);
        }
        // sleep in small steps so a drop ends the thread promptly
        while Instant::now() < next && !shared.stopped() {
            std::thread::sleep((next - Instant::now()).min(Duration::from_millis(200)));
        }
    }
}

// ------------------------------------------------------------------ gpu

fn gpu_loop(shared: Arc<Shared>, waker: Waker) {
    while !shared.stopped() {
        if !shared.active() {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let st = query_gpu();
        let missing = matches!(st, GpuState::Missing);
        {
            let mut g = shared.lock();
            if let GpuState::Ok(gpu) = &st {
                push(&mut g.gpu_hist, gpu.util);
            }
            g.gpu = st;
        }
        waker.wake();
        if missing {
            return;
        }
        for _ in 0..10 {
            if shared.stopped() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

pub fn query_gpu() -> GpuState {
    let args = [
        "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,fan.speed",
        "--format=csv,noheader,nounits",
    ];
    match super::probes::run_cmd("nvidia-smi", &args) {
        Ok(text) => parse_gpu(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GpuState::Missing,
        Err(_) => GpuState::Failed,
    }
}

pub fn parse_gpu(text: &str) -> GpuState {
    let Some(line) = text.lines().next() else { return GpuState::Failed };
    let f: Vec<&str> = line.split(',').map(|x| x.trim()).collect();
    if f.len() < 8 {
        return GpuState::Failed;
    }
    let num = |s: &str| s.parse::<f64>().unwrap_or(0.0);
    GpuState::Ok(Gpu {
        name: f[0].to_string(),
        util: num(f[1]) as f32,
        used: num(f[2]),
        total: num(f[3]),
        temp: num(f[4]) as f32,
        power: num(f[5]) as f32,
        limit: num(f[6]) as f32,
        fan: num(f[7]) as f32,
    })
}

/// Terminate a process (SIGTERM on Unix, TerminateProcess on Windows).
pub fn kill(pid: u32) -> Result<(), String> {
    let mut s = sysinfo::System::new();
    let p = Pid::from_u32(pid);
    s.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing().without_tasks());
    let Some(proc) = s.process(p) else { return Err("it's already gone".into()) };
    match proc.kill_with(sysinfo::Signal::Term) {
        Some(true) => Ok(()),
        Some(false) => Err("access denied".into()),
        None => {
            if proc.kill() {
                Ok(())
            } else {
                Err("access denied".into())
            }
        }
    }
}
