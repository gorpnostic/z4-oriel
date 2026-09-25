//! The system app's background sampling. Everything slow happens here, off the UI thread: sysinfo refreshes
//! (CPU, memory, disks, network, ~700 processes), the Wren trainer lookup, and `nvidia-smi` on its own thread.
//! Each second the sampler publishes an immutable `Snap` behind an `Arc` and wakes the UI, which only swaps the
//! pointer and draws. Sampling pauses by itself when the UI stops polling (pane hidden) and ends on drop.

use crate::pane::Waker;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{
    CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, Networks, Pid, ProcessRefreshKind, ProcessesToUpdate,
    RefreshKind, Uid, UpdateKind, Users,
};

/// Seconds of history the sparklines keep.
pub const HISTORY: usize = 120;

/// Where Wren lives. The training card only shows a `train.py` whose working directory is this folder (other
/// projects train too), and reads `<root>/checkpoints/progress.json` + `<mode>_log.jsonl`.
#[cfg(windows)]
pub const WREN_ROOT: &str = r"C:\Code\ai\wren";
#[cfg(not(windows))]
pub const WREN_ROOT: &str = "~/Code/ai/wren";

const TRAIN_MODES: &[&str] = &["pretrain", "sft", "polish", "extend", "web", "topical", "variety", "websight"];

pub fn wren_root() -> PathBuf {
    match WREN_ROOT.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(WREN_ROOT),
    }
}

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
}

#[derive(Clone, Debug)]
pub struct DiskRow {
    pub label: String,
    pub total: u64,
    pub free: u64,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub step: u64,
    pub total: u64,
    pub loss: f64,
    pub tok_s: f64,
    pub eta: f64,
}

#[derive(Clone, Debug)]
pub struct Eval {
    pub step: u64,
    pub val: f64,
    pub time: f64,
}

#[derive(Clone, Debug)]
pub struct Train {
    pub mode: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub progress: Option<Progress>,
    pub evals: Arc<Vec<Eval>>,
    /// (prompt, output) pairs from the latest eval.
    pub samples: Arc<Vec<(String, String)>>,
}

#[derive(Clone, Debug, Default)]
pub struct Snap {
    pub seq: u64,
    pub cpu_hist: VecDeque<f32>,
    pub cores: Vec<f32>,
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
    pub train: Option<Train>,
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
    fn stopped(&self) -> bool {
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
    /// python pid -> Some(mode) if it is Wren's trainer; each python is inspected once.
    py_seen: HashMap<u32, Option<String>>,
    last_prune: Instant,
    ncpu: f32,
    hist: Snap,
    last: Instant,
    last_net: Option<(u64, u64)>,
    last_io: Option<(u64, u64)>,
    last_freq: Option<Instant>,
    last_disk_list: Instant,
    #[cfg(windows)]
    prev_cpu: HashMap<u32, u64>,
    log_cache: Option<(PathBuf, SystemTime, u64, Arc<Vec<Eval>>, Arc<Vec<(String, String)>>)>,
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
            py_seen: HashMap::new(),
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
            log_cache: None,
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
        let total = if cores.is_empty() { 0.0 } else { cores.iter().sum::<f32>() / cores.len() as f32 };
        push(&mut self.hist.cpu_hist, total);
        let freqs: Vec<u64> = self.sys.cpus().iter().map(|c| c.frequency()).filter(|&f| f > 0).collect();
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
            self.py_seen.retain(|k, _| live.contains(k));
        }

        let train = self.training(&procs);
        let mut s = self.hist.clone();
        s.seq = self.hist.seq + 1;
        self.hist.seq = s.seq;
        s.cores = cores;
        s.ghz = ghz;
        s.ram_used = ram_used;
        s.ram_total = ram_total;
        s.disks = disks;
        s.procs = procs;
        s.train = train;
        s.sample_ms = t0.elapsed().as_secs_f64() * 1000.0;
        s
    }

    /// Every process. Windows: one NtQuerySystemInformation call gives name, threads, working set and CPU time
    /// for all of them (~5 ms); sysinfo's own per-process CPU refresh opens every process and cost ~80 ms here.
    /// sysinfo is only asked for the user of processes we haven't seen before.
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
            cpu_now.insert(r.pid, r.cpu_time);
            if r.pid == 0 {
                continue; // System Idle Process
            }
            let cpu = self.prev_cpu.get(&r.pid).map(|&c| r.cpu_time.saturating_sub(c) as f64 * scale).unwrap_or(0.0);
            procs.push(Proc {
                pid: r.pid,
                cpu: cpu.min(100.0) as f32,
                mem: r.mem,
                threads: r.threads,
                user: self.user_of.get(&r.pid).cloned().unwrap_or_else(|| self.empty.clone()),
                name: r.name,
            });
        }
        self.prev_cpu = cpu_now;
        procs
    }

    #[cfg(not(windows))]
    fn processes(&mut self, _dt: f64) -> Vec<Proc> {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory().with_user(UpdateKind::OnlyIfNotSet).without_tasks(),
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
            procs.push(Proc { pid, name: p.name().to_string_lossy().into_owned(), cpu: p.cpu_usage() / self.ncpu, mem: p.memory(), threads: proc_threads(pid), user });
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

    /// Wren's trainer: a python process running train.py with Wren's folder as its working directory, plus the
    /// progress file it writes every 20 steps. None when neither is there (the card hides).
    fn training(&mut self, procs: &[Proc]) -> Option<Train> {
        let root = wren_root();
        let pythons: Vec<u32> = procs.iter().filter(|p| p.name.to_ascii_lowercase().starts_with("python")).map(|p| p.pid).collect();
        // command line + cwd only for pythons we haven't looked at yet
        let new: Vec<Pid> = pythons.iter().filter(|p| !self.py_seen.contains_key(p)).map(|&p| Pid::from_u32(p)).collect();
        if !new.is_empty() {
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&new),
                false,
                ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet).with_cwd(UpdateKind::OnlyIfNotSet).without_tasks(),
            );
            for pid in &new {
                let mode = self.sys.process(*pid).and_then(|p| {
                    let cmd: Vec<String> = p.cmd().iter().map(|c| c.to_string_lossy().replace('\\', "/")).collect();
                    if !cmd.iter().any(|c| c.ends_with("train.py")) || !p.cwd().is_some_and(|c| same_path(c, &root)) {
                        return None;
                    }
                    Some(cmd.iter().find(|c| TRAIN_MODES.contains(&c.as_str()) || c.starts_with("v2-")).cloned().unwrap_or_else(|| "train".into()))
                });
                self.py_seen.insert(pid.as_u32(), mode);
            }
        }
        self.py_seen.retain(|k, _| pythons.contains(k));
        let found = pythons.iter().find_map(|p| self.py_seen.get(p).cloned().flatten().map(|m| (*p, m)));
        let ckpt = root.join("checkpoints");
        let mut progress = None;
        let mut pmode = None;
        if let Ok(txt) = std::fs::read_to_string(ckpt.join("progress.json"))
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt)
        {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            if now - v["time"].as_f64().unwrap_or(0.0) <= 300.0 {
                progress = Some(Progress {
                    step: v["step"].as_u64().unwrap_or(0),
                    total: v["total"].as_u64().unwrap_or(0),
                    loss: v["loss"].as_f64().unwrap_or(0.0),
                    tok_s: v["tok_s"].as_f64().unwrap_or(0.0),
                    eta: v["eta"].as_f64().unwrap_or(0.0),
                });
                pmode = v["mode"].as_str().map(String::from);
            }
        }
        if found.is_none() && progress.is_none() {
            return None;
        }
        let mode = found.as_ref().map(|f| f.1.clone()).or(pmode).unwrap_or_else(|| "train".into());
        let (evals, samples) = self.evals(&ckpt.join(format!("{mode}_log.jsonl")));
        Some(Train { mode, running: found.is_some(), pid: found.map(|f| f.0), progress, evals, samples })
    }

    /// The eval log (one JSON line every 250 steps), re-read only when the file changes.
    fn evals(&mut self, path: &Path) -> (Arc<Vec<Eval>>, Arc<Vec<(String, String)>>) {
        let meta = std::fs::metadata(path).ok();
        let (mtime, len) = meta.map(|m| (m.modified().unwrap_or(UNIX_EPOCH), m.len())).unwrap_or((UNIX_EPOCH, 0));
        if let Some((p, t, l, e, s)) = &self.log_cache
            && p == path
            && *t == mtime
            && *l == len
        {
            return (e.clone(), s.clone());
        }
        let mut evals = vec![];
        let mut samples = vec![];
        if let Ok(txt) = std::fs::read_to_string(path) {
            for line in txt.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let (Some(step), Some(val)) = (v["step"].as_u64(), v["val"].as_f64()) else { continue };
                evals.push(Eval { step, val, time: v["time"].as_f64().unwrap_or(0.0) });
                if let Some(arr) = v["samples"].as_array() {
                    samples = arr
                        .iter()
                        .filter_map(|s| Some((s.get(0)?.as_str()?.to_string(), s.get(1)?.as_str()?.to_string())))
                        .collect();
                }
            }
        }
        let (e, s) = (Arc::new(evals), Arc::new(samples));
        self.log_cache = Some((path.to_path_buf(), mtime, len, e.clone(), s.clone()));
        (e, s)
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

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        let s = s.trim_end_matches('/').to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
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
    }

    #[repr(C)]
    struct UnicodeString {
        len: u16,
        _max: u16,
        buf: *const u16,
    }
    // SYSTEM_PROCESS_INFORMATION (64-bit layout), up to WorkingSetSize
    #[repr(C)]
    struct Spi {
        next: u32,
        threads: u32,
        _private_ws: i64,
        _hard_faults: u32,
        _threads_hwm: u32,
        _cycle: u64,
        _create: i64,
        user: i64,
        kernel: i64,
        name: UnicodeString,
        _prio: i32,
        pid: usize,
        _parent: usize,
        _handles: u32,
        session: u32,
        _key: usize,
        _peak_virtual: usize,
        _virtual: usize,
        _page_faults: u32,
        _peak_ws: usize,
        ws: usize,
    }
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
                out.push(Raw { pid: e.pid as u32, name, threads: e.threads, mem: e.ws as u64, cpu_time: (e.user + e.kernel).max(0) as u64, session: e.session });
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
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,fan.speed",
        "--format=csv,noheader,nounits",
    ])
    .stdin(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return GpuState::Missing,
        Err(_) => return GpuState::Failed,
    };
    parse_gpu(&String::from_utf8_lossy(&out.stdout))
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
