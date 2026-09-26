//! The event center: things worth knowing that happened while you were busy (an agent finished or needs you,
//! an approval is waiting, a build failed, a plan is about to start, an install finished, memory is running out,
//! an AI's usage is near its limit, a new version is out). Each one is a toast, a line in the alerts app, and,
//! when the terminal isn't the window you're in, a desktop notification. Above them sits what's open right now
//! (`Open`): live state the panes report, not events, so it clears itself once answered.
//!
//! Kept in alerts.json, which every open oriel window shares: saving reads the file first and merges, so one
//! window never wipes another's alerts, and the new file is swapped in whole, so a crash can't leave it torn.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    AgentDone,
    NeedsYou,
    Approval,
    BuildFailed,
    Calendar,
    Download,
    Memory,
    Usage,
    Update,
}

impl Kind {
    /// (glyph, short name) for the list.
    pub fn label(self) -> (&'static str, &'static str) {
        match self {
            Kind::AgentDone => ("●", "finished"),
            Kind::NeedsYou => ("●", "needs you"),
            Kind::Approval => ("?", "asking you"),
            Kind::BuildFailed => ("×", "failed"),
            Kind::Calendar => ("◷", "calendar"),
            Kind::Download => ("↓", "installed"),
            Kind::Memory => ("▲", "memory"),
            Kind::Usage => ("◔", "usage"),
            Kind::Update => ("↑", "update"),
        }
    }
    /// Its name in the config (`[alerts] desktop = ["needs_you", …]`).
    pub fn id(self) -> &'static str {
        match self {
            Kind::AgentDone => "agent_done",
            Kind::NeedsYou => "needs_you",
            Kind::Approval => "approval",
            Kind::BuildFailed => "build_failed",
            Kind::Calendar => "calendar",
            Kind::Download => "download",
            Kind::Memory => "memory",
            Kind::Usage => "usage",
            Kind::Update => "update",
        }
    }
    /// Worth a desktop notification when you're in another window: desktop notifications are on and this kind
    /// is one you picked (settings › alerts).
    pub fn loud(self, cfg: &crate::config::Config) -> bool {
        cfg.desktop_notifications && cfg.alerts.desktop.iter().any(|k| k == self.id())
    }
}

pub const KINDS: &[Kind] = &[Kind::AgentDone, Kind::NeedsYou, Kind::Approval, Kind::BuildFailed, Kind::Calendar, Kind::Download, Kind::Memory, Kind::Usage, Kind::Update];

/// The watcher's thresholds, shared with the app so a change in settings applies without a restart.
pub struct Thresholds {
    pub memory_pct: AtomicU32,
    pub usage_pct: AtomicU32,
    pub weekly_pct: AtomicU32,
}

impl Thresholds {
    pub fn new(c: &crate::config::AlertsConfig) -> Arc<Thresholds> {
        let t = Arc::new(Thresholds { memory_pct: AtomicU32::new(0), usage_pct: AtomicU32::new(0), weekly_pct: AtomicU32::new(0) });
        t.set(c);
        t
    }
    pub fn set(&self, c: &crate::config::AlertsConfig) {
        self.memory_pct.store(c.memory_pct.clamp(10, 100), Ordering::Relaxed);
        self.usage_pct.store(c.usage_pct.clamp(1, 100), Ordering::Relaxed);
        self.weekly_pct.store(c.weekly_usage_pct.clamp(1, 100), Ordering::Relaxed);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Alert {
    pub at: i64,
    pub kind: Kind,
    pub text: String,
    /// the app it came from, to jump back to
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub read: bool,
    /// the pane it came from (this session only)
    #[serde(skip)]
    pub pane: Option<u64>,
}

/// An alert's identity, the same in every window and after a restart.
pub type Key = (i64, Kind, String);

pub fn key(a: &Alert) -> Key {
    (a.at, a.kind, a.text.clone())
}

/// The newest this many are kept.
const KEEP: usize = 200;

#[derive(Default)]
struct Center {
    /// newest last
    list: Vec<Alert>,
    /// the file as this window last read or wrote it: what's gone from it since, another window dismissed
    base: HashSet<Key>,
    /// the file it's kept in (None = nowhere: tests that don't give one)
    file: Option<PathBuf>,
    /// the file's modified time when we last read or wrote it, to notice another window's writes
    seen: Option<std::time::SystemTime>,
}

thread_local! {
    /// Only the UI thread adds, reads, marks and dismisses (and thread-local keeps parallel tests apart).
    static CENTER: RefCell<Center> = RefCell::new(Center::default());
}

/// Start keeping alerts in oriel's data folder, with what's there already (the app, once at start).
pub fn open() {
    open_at(crate::config::data_dir().join("alerts.json"));
}

pub fn open_at(file: PathBuf) {
    CENTER.with_borrow_mut(|c| {
        c.list = read(&file).unwrap_or_default();
        c.base = c.list.iter().map(key).collect();
        c.seen = modified(&file);
        c.file = Some(file);
    });
}

pub fn unread() -> usize {
    CENTER.with_borrow(|c| c.list.iter().filter(|a| !a.read).count())
}

/// Look at every alert, newest last.
pub fn with<R>(f: impl FnOnce(&[Alert]) -> R) -> R {
    CENTER.with_borrow(|c| f(&c.list))
}

pub fn push(a: Alert) {
    CENTER.with_borrow_mut(|c| {
        // the same thing again while the first is still unread ("oriel 0.8 is out" at every start): move it up
        // instead of listing it twice. For updates, a newer version replaces the older one.
        let same = |b: &Alert| !b.read && b.kind == a.kind && (b.text == a.text || (a.kind == Kind::Update && b.app == a.app));
        c.list.retain(|b| !same(b));
        c.list.push(a);
        let n = c.list.len();
        c.list.drain(..n.saturating_sub(KEEP));
        save(c);
    });
}

/// You've seen them (the alerts app is on screen).
pub fn mark_all_read() {
    CENTER.with_borrow_mut(|c| {
        if c.list.iter().any(|a| !a.read) {
            c.list.iter_mut().for_each(|a| a.read = true);
            save(c);
        }
    });
}

/// Mark the ones `f` picks as read (alt j went to them).
pub fn mark_read_if(f: impl Fn(&Alert) -> bool) {
    CENTER.with_borrow_mut(|c| {
        let mut any = false;
        for a in c.list.iter_mut().filter(|a| !a.read && f(a)) {
            a.read = true;
            any = true;
        }
        if any {
            save(c);
        }
    });
}

pub fn dismiss(k: &Key) {
    CENTER.with_borrow_mut(|c| {
        c.list.retain(|a| key(a) != *k);
        save(c);
    });
}

pub fn clear() {
    CENTER.with_borrow_mut(|c| {
        c.list.clear();
        save(c);
    });
}

/// Another window saved since we last looked: take in what it added (the alerts app, as it draws).
pub fn sync() {
    CENTER.with_borrow_mut(|c| {
        let Some(file) = c.file.clone() else { return };
        let m = modified(&file);
        if m.is_some() && m != c.seen {
            let theirs = read(&file);
            c.list = merge(&c.list, &c.base, theirs.as_deref());
            if let Some(t) = theirs {
                c.base = t.iter().map(key).collect();
            }
            c.seen = m;
        }
    });
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn modified(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// The file's alerts. One that doesn't parse (torn by a crash) is moved aside rather than read as empty and
/// then overwritten.
fn read(file: &Path) -> Option<Vec<Alert>> {
    let s = std::fs::read_to_string(file).ok()?;
    match serde_json::from_str(&s) {
        Ok(v) => Some(v),
        Err(_) => {
            let _ = std::fs::rename(file, file.with_extension("json.bad"));
            None
        }
    }
}

/// Write it out without losing another window's alerts: read the file, merge, then swap the new file in whole.
/// (Small and rare: a new alert, a dismissal, a look at the alerts app.)
fn save(c: &mut Center) {
    let Some(file) = c.file.clone() else { return };
    c.list = merge(&c.list, &c.base, read(&file).as_deref());
    if let Ok(j) = serde_json::to_vec(&c.list) {
        if write_atomic(&file, &j).is_ok() {
            c.base = c.list.iter().map(key).collect();
        }
    }
    c.seen = modified(&file);
}

/// Three ways: `mine` (this window's list), `base` (the file as this window last saw it) and `theirs` (the file
/// now). What's new on either side stays, what either side dismissed or cleared goes, read in either is read.
/// An unreadable or missing file says nothing about what anyone removed: then it's just `mine`.
fn merge(mine: &[Alert], base: &HashSet<Key>, theirs: Option<&[Alert]>) -> Vec<Alert> {
    let Some(theirs) = theirs else { return mine.to_vec() };
    let on_disk: HashSet<Key> = theirs.iter().map(key).collect();
    // here before, gone from the file now: another window dismissed it
    let mut out: Vec<Alert> = mine.iter().filter(|a| !base.contains(&key(a)) || on_disk.contains(&key(a))).cloned().collect();
    let mut have: HashMap<Key, usize> = out.iter().enumerate().map(|(i, a)| (key(a), i)).collect();
    for d in theirs {
        let k = key(d);
        if let Some(&i) = have.get(&k) {
            out[i].read |= d.read;
            if out[i].app.is_none() {
                out[i].app = d.app.clone();
            }
        } else if !base.contains(&k) {
            // new from another window (in the base but not in mine = this window dismissed it)
            have.insert(k, out.len());
            out.push(d.clone());
        }
    }
    out.sort_by_key(|a| a.at);
    let n = out.len();
    out.drain(..n.saturating_sub(KEEP));
    out
}

/// Replace a file in one step (a temp file renamed over it): a crash mid-write leaves the old one whole.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

// ------------------------------------------------------------------ open now
/// Something waiting on you right now. Not an event: live state, gathered again from the panes before every
/// draw (a question or approval a chat is holding, a stuck or finished task, a terminal agent at a prompt), so
/// it goes away by itself once it's answered. The alerts app lists these above the history and answers them.
#[derive(Clone, Debug, PartialEq)]
pub struct Open {
    /// Approval (a question or a yes/no), NeedsYou (stuck, go look) or AgentDone (finished: ready to review)
    pub kind: Kind,
    /// the pane's own name for it, handed back with an answer so an answer can't land on a newer question
    pub key: String,
    /// one line for the list
    pub text: String,
    /// the peek (space): the whole question, or the result card
    pub detail: Vec<String>,
    /// the choices 1-9 answer it with, ticked ones marked when several can be picked
    pub options: Vec<String>,
    pub multi: bool,
    pub ticked: Vec<bool>,
    /// y / n answers it (an approval)
    pub yes_no: bool,
    /// the tagged tab it's about (a task's terminal): that tab's own red dot isn't listed twice
    pub tag: Option<String>,
    // ---- filled in by the app
    /// the pane that holds it (answers go there, enter goes there), the tab it's in, and its app tab if any
    pub pane: u64,
    pub from: String,
    pub app: Option<&'static str>,
}

impl Open {
    pub fn new(kind: Kind, key: impl Into<String>, text: impl Into<String>) -> Open {
        Open { kind, key: key.into(), text: text.into(), detail: vec![], options: vec![], multi: false, ticked: vec![], yes_no: false, tag: None, pane: 0, from: String::new(), app: None }
    }
    /// Waiting on you, rather than done and waiting to be looked at.
    pub fn needs_you(&self) -> bool {
        self.kind != Kind::AgentDone
    }
    /// (glyph, short name) for the list.
    pub fn label(&self) -> (&'static str, &'static str) {
        match self.kind {
            Kind::AgentDone => ("◆", "review"),
            k => k.label(),
        }
    }
}

/// How the alerts app answers an open item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// choice n (0-based); ticks it when several can be picked
    Pick(usize),
    /// allow, or send the ticked choices
    Yes,
    No,
    /// show it (the app has already switched to its pane)
    Go,
}

thread_local! {
    static OPEN: RefCell<Vec<Open>> = const { RefCell::new(vec![]) };
}

/// The app's latest gathering: waiting on you first, then ready to review.
pub fn set_open(mut l: Vec<Open>) {
    l.sort_by_key(|o| !o.needs_you());
    OPEN.with_borrow_mut(|o| {
        if *o != l {
            *o = l;
        }
    });
}

pub fn with_open<R>(f: impl FnOnce(&[Open]) -> R) -> R {
    OPEN.with_borrow(|o| f(o))
}

// ------------------------------------------------------------------ what's been said, across restarts
/// What the watchers and the update check have already said, shared by every window and kept across restarts,
/// so none of it is said again each time oriel starts.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct State {
    /// usage warnings sent: (agent, limit window, when it resets; 0 = unknown)
    pub usage: Vec<(String, String, i64)>,
    /// memory is over the line and that was said (it can be said again once it drops back)
    pub memory: bool,
    /// the version an "is out" alert was last raised for
    pub announced: Option<String>,
    /// the version you rolled back from: not offered again
    pub skipped: Option<String>,
}

fn state_file() -> PathBuf {
    if cfg!(test) {
        return std::path::absolute("target/test-scratch/shell/alerts-state.json").unwrap_or_default();
    }
    crate::config::data_dir().join("alerts-state.json")
}

pub fn state() -> State {
    std::fs::read_to_string(state_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Read, change and write back the state file.
pub fn update_state(f: impl FnOnce(&mut State)) {
    let mut s = state();
    f(&mut s);
    if let Ok(j) = serde_json::to_vec(&s) {
        let _ = write_atomic(&state_file(), &j);
    }
}

/// Which of `limits` (agent, window, % used, resets at) to warn about now: over the line and not said yet for
/// this window of the limit. `said` remembers; a window drops out of it once it resets (or, with no reset time,
/// once it's back under the line). The lines are (% for a session window, % for the weekly one), from settings.
fn usage_due(limits: &[(String, String, f64, Option<i64>)], said: &mut Vec<(String, String, i64)>, now: i64, lines: (f64, f64)) -> Vec<usize> {
    let line = |label: &str| if label == "weekly" { lines.1 } else { lines.0 };
    said.retain(|(agent, label, resets)| *resets > now || (*resets == 0 && limits.iter().any(|l| l.0 == *agent && l.1 == *label && l.2 >= line(label))));
    let mut due = vec![];
    for (i, (agent, label, pct, resets)) in limits.iter().enumerate() {
        let k = (agent.clone(), label.clone(), resets.unwrap_or(0));
        if *pct >= line(label) && !said.contains(&k) {
            said.push(k);
            due.push(i);
        }
    }
    due
}

/// "just now", "5m", "3h", "2d".
pub fn ago(at: i64) -> String {
    let s = (now() - at).max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// A notification from the OS: Windows' toast, or notify-send on Linux. Never blocks, never in tests.
pub fn desktop(title: &str, body: &str) {
    if cfg!(test) {
        return;
    }
    let (title, body) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\'', "&apos;").replace('"', "&quot;");
            // Windows PowerShell 5.1 shows toasts through WinRT, under its own app id
            let script = format!(
                "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;\
                 [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null;\
                 $x = New-Object Windows.Data.Xml.Dom.XmlDocument;\
                 $x.LoadXml('<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>');\
                 [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($x))",
                esc(&title),
                esc(&body)
            );
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
            let ps = format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
            let _ = std::process::Command::new(ps)
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                // a pwsh 7 parent's module path breaks 5.1's own modules
                .env("PSModulePath", format!("{root}\\System32\\WindowsPowerShell\\v1.0\\Modules"))
                .creation_flags(0x08000000)
                .output();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("notify-send").args(["-a", "oriel", &title, &body]).output();
        }
    });
}

// ------------------------------------------------------------------ watchers that run in the background
/// Memory running out, and AI usage near its limit: checked every half minute / five minutes, each said once
/// until it clears (or the usage window resets), counting what earlier runs and other windows already said.
/// The thresholds are read on every check (settings › alerts).
pub fn watch(th: Arc<Thresholds>, send: impl Fn(Kind, String) + Send + 'static) {
    if cfg!(test) {
        return;
    }
    std::thread::spawn(move || {
        use sysinfo::System;
        let mut sys = System::new();
        let mut tick = 0u64;
        loop {
            sys.refresh_memory();
            let (used, total) = (sys.used_memory(), sys.total_memory().max(1));
            let pct = used as f64 / total as f64 * 100.0;
            let warn_at = th.memory_pct.load(Ordering::Relaxed) as f64;
            let said = state().memory;
            if pct > warn_at && !said {
                update_state(|s| s.memory = true);
                send(Kind::Memory, format!("memory is {pct:.0}% full ({} of {}): the system app shows what's using it", crate::ui::human_bytes(used), crate::ui::human_bytes(total)));
            } else if pct < warn_at - 7.0 && said {
                update_state(|s| s.memory = false);
            }
            if tick % 10 == 0 {
                let limits: Vec<(String, String, f64, Option<i64>)> = crate::panes::agents::usage_limits().into_iter().map(|(a, w)| (a, w.label, w.pct, w.resets_at)).collect();
                let lines = (th.usage_pct.load(Ordering::Relaxed) as f64, th.weekly_pct.load(Ordering::Relaxed) as f64);
                let mut due = vec![];
                update_state(|s| due = usage_due(&limits, &mut s.usage, now(), lines));
                for i in due {
                    let (agent, label, pct, resets) = &limits[i];
                    let when = resets.map(|r| format!(", resets in {}", crate::panes::agents::until(r))).unwrap_or_default();
                    send(Kind::Usage, format!("{agent}: {pct:.0}% of your {label} limit used{when}"));
                }
            }
            tick += 1;
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which kinds make a desktop notification is a setting ("needs you" but not "finished", by default), and
    /// the master switch still wins; the thresholds reach the watcher through shared atomics.
    #[test]
    fn alerts_desktop_kinds_and_thresholds() {
        let mut c = crate::config::Config::default();
        assert!(Kind::NeedsYou.loud(&c) && Kind::BuildFailed.loud(&c));
        assert!(!Kind::AgentDone.loud(&c) && !Kind::Update.loud(&c));
        c.alerts.desktop.push("agent_done".into());
        assert!(Kind::AgentDone.loud(&c));
        c.desktop_notifications = false;
        assert!(!Kind::NeedsYou.loud(&c), "off is off");
        assert_eq!(KINDS.len(), 9);
        assert!(KINDS.iter().all(|k| serde_json::to_string(k).unwrap() == format!("\"{}\"", k.id())), "the config names match the saved ones");
        let th = Thresholds::new(&c.alerts);
        assert_eq!(th.memory_pct.load(Ordering::Relaxed), 92);
        c.alerts.memory_pct = 80;
        c.alerts.weekly_usage_pct = 95;
        th.set(&c.alerts);
        assert_eq!((th.memory_pct.load(Ordering::Relaxed), th.weekly_pct.load(Ordering::Relaxed)), (80, 95));
    }

    fn alert(at: i64, kind: Kind, text: &str) -> Alert {
        Alert { at, kind, text: text.into(), app: None, read: false, pane: None }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::path::absolute("target/test-scratch/shell").unwrap();
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join(name);
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_file(f.with_extension("json.bad"));
        f
    }

    fn on_disk(f: &Path) -> Vec<Alert> {
        serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap()
    }

    fn texts(l: &[Alert]) -> Vec<String> {
        l.iter().map(|a| a.text.clone()).collect()
    }

    #[test]
    fn alerts_same_one_twice_is_listed_once() {
        CENTER.with_borrow_mut(|c| *c = Center::default());
        push(alert(100, Kind::Update, "oriel 0.8.0 is out"));
        push(alert(200, Kind::Update, "oriel 0.8.0 is out"));
        push(alert(300, Kind::AgentDone, "claude finished"));
        push(alert(400, Kind::AgentDone, "claude finished"));
        push(alert(500, Kind::Update, "oriel 0.8.1 is out"));
        let got = with(|l| l.iter().map(|a| (a.at, a.text.clone())).collect::<Vec<_>>());
        assert_eq!(got, vec![(400, "claude finished".to_string()), (500, "oriel 0.8.1 is out".to_string())], "moved up, not repeated");
        // once read, the same thing again is news again
        mark_all_read();
        push(alert(600, Kind::AgentDone, "claude finished"));
        assert_eq!(with(|l| l.len()), 3);
        assert_eq!(unread(), 1);
    }

    #[test]
    fn alerts_two_windows_share_the_file() {
        let f = scratch("alerts-two-windows.json");
        // window A records two alerts
        CENTER.with_borrow_mut(|c| *c = Center::default());
        open_at(f.clone());
        push(alert(100, Kind::AgentDone, "a1"));
        push(alert(110, Kind::Calendar, "a2"));
        let a = CENTER.with_borrow_mut(std::mem::take);
        // window B, started before them, gets one of its own: A's two survive B's save
        CENTER.with_borrow_mut(|c| c.file = Some(f.clone()));
        push(alert(120, Kind::Download, "b1"));
        assert_eq!(texts(&on_disk(&f)), ["a1", "a2", "b1"]);
        // B dismisses a1 and reads the rest
        let k = with(|l| key(&l[0]));
        dismiss(&k);
        mark_all_read();
        assert_eq!(texts(&on_disk(&f)), ["a2", "b1"]);
        let b = CENTER.with_borrow_mut(std::mem::take);
        // back in A: it sees B's b1, drops the a1 B dismissed, and keeps B's read flags
        CENTER.with_borrow_mut(|c| *c = a);
        sync();
        assert_eq!(with(texts), ["a2", "b1"]);
        push(alert(130, Kind::Memory, "a3"));
        assert_eq!(texts(&on_disk(&f)), ["a2", "b1", "a3"], "a1 stays dismissed");
        assert!(on_disk(&f).iter().any(|a| a.text == "b1" && a.read), "B's read flag kept");
        // B clears all it has seen: A's a3, newer than B's last look, survives it
        CENTER.with_borrow_mut(|c| *c = b);
        clear();
        assert_eq!(texts(&on_disk(&f)), ["a3"]);
        CENTER.with_borrow_mut(|c| *c = Center::default());
    }

    #[test]
    fn alerts_torn_file_is_kept_aside() {
        let f = scratch("alerts-torn.json");
        std::fs::write(&f, r#"[{"at": 1, "kind": "agent_do"#).unwrap();
        CENTER.with_borrow_mut(|c| *c = Center::default());
        open_at(f.clone());
        assert_eq!(unread(), 0);
        assert!(f.with_extension("json.bad").exists(), "the torn file is moved aside, not overwritten");
        push(alert(5, Kind::Calendar, "after"));
        assert_eq!(texts(&on_disk(&f)), ["after"]);
        let dir = f.parent().unwrap();
        assert!(std::fs::read_dir(dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("alerts-torn.tmp")), "no temp file left");
        CENTER.with_borrow_mut(|c| *c = Center::default());
    }

    #[test]
    fn alerts_usage_warned_once_per_window() {
        let limits = |pct: f64| vec![("claude".to_string(), "5-hour".to_string(), pct, Some(1000)), ("codex".to_string(), "weekly".to_string(), 88.0, None)];
        let mut said = vec![];
        assert_eq!(usage_due(&limits(87.0), &mut said, 500, (85.0, 90.0)), vec![0], "87% of 5-hour: warn; 88% weekly is under its 90% line");
        // a restart (or another window) with the same state: quiet
        let mut again = said.clone();
        assert!(usage_due(&limits(95.0), &mut again, 600, (85.0, 90.0)).is_empty());
        // the window reset: the new one can warn again
        let mut later = said.clone();
        let next = vec![("claude".to_string(), "5-hour".to_string(), 90.0, Some(3000))];
        assert_eq!(usage_due(&next, &mut later, 1500, (85.0, 90.0)), vec![0]);
        assert!(!later.iter().any(|k| k.2 == 1000), "the old window dropped out");
    }

    #[test]
    fn alerts_open_needs_you_first() {
        set_open(vec![Open::new(Kind::AgentDone, "task:1", "fix tests is ready for review"), Open::new(Kind::Approval, "q:x", "claude asks: which?"), Open::new(Kind::NeedsYou, "term", "claude is waiting")]);
        let got = with_open(|l| l.iter().map(|o| (o.key.clone(), o.needs_you())).collect::<Vec<_>>());
        assert_eq!(got, [("q:x".to_string(), true), ("term".to_string(), true), ("task:1".to_string(), false)], "stable: waiting on you, then review");
        assert_eq!(with_open(|l| l[2].label().1), "review");
        set_open(vec![]);
        assert!(with_open(|l| l.is_empty()));
    }
}
