//! The event center: things worth knowing that happened while you were busy (an agent finished or needs you,
//! an approval is waiting, a build failed, a plan is about to start, an install finished, memory is running out,
//! an AI's usage is near its limit, a new version is out). Each one is a toast, a line in the alerts app, and,
//! when the terminal isn't the window you're in, a desktop notification.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
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

/// Everything so far, newest last (the app adds; the alerts app reads, marks read, dismisses).
pub static CENTER: std::sync::Mutex<Vec<Alert>> = std::sync::Mutex::new(Vec::new());

pub fn unread() -> usize {
    CENTER.lock().unwrap().iter().filter(|a| !a.read).count()
}

pub fn push(a: Alert) {
    let mut c = CENTER.lock().unwrap();
    c.push(a);
    let n = c.len();
    if n > 200 {
        c.drain(..n - 200);
    }
    save(&c);
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn file() -> PathBuf {
    if cfg!(test) {
        return std::path::absolute("target/test-scratch/alerts.json").unwrap_or_default();
    }
    crate::config::data_dir().join("alerts.json")
}

pub fn load() -> Vec<Alert> {
    std::fs::read_to_string(file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(all: &[Alert]) {
    let keep = &all[all.len().saturating_sub(200)..];
    let _ = std::fs::write(file(), serde_json::to_string(keep).unwrap_or_default());
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
/// until it clears (or the usage window resets). The thresholds are read on every check (settings › alerts).
pub fn watch(th: Arc<Thresholds>, send: impl Fn(Kind, String) + Send + 'static) {
    if cfg!(test) {
        return;
    }
    std::thread::spawn(move || {
        use sysinfo::System;
        let mut sys = System::new();
        let mut mem_warned = false;
        let mut usage_warned: std::collections::HashSet<(String, String, i64)> = Default::default();
        let mut tick = 0u64;
        loop {
            sys.refresh_memory();
            let (used, total) = (sys.used_memory(), sys.total_memory().max(1));
            let pct = used as f64 / total as f64 * 100.0;
            let warn_at = th.memory_pct.load(Ordering::Relaxed) as f64;
            if pct > warn_at && !mem_warned {
                mem_warned = true;
                send(Kind::Memory, format!("memory is {pct:.0}% full ({} of {}): the system app shows what's using it", crate::ui::human_bytes(used), crate::ui::human_bytes(total)));
            } else if pct < warn_at - 7.0 {
                mem_warned = false;
            }
            if tick % 10 == 0 {
                let limits = crate::panes::agents::usage_limits();
                for (agent, w) in limits {
                    let warn = if w.label == "weekly" { th.weekly_pct.load(Ordering::Relaxed) } else { th.usage_pct.load(Ordering::Relaxed) } as f64;
                    let key = (agent.clone(), w.label.clone(), w.resets_at.unwrap_or(0));
                    if w.pct >= warn && usage_warned.insert(key) {
                        let when = w.resets_at.map(|r| format!(", resets in {}", crate::panes::agents::until(r))).unwrap_or_default();
                        send(Kind::Usage, format!("{agent}: {:.0}% of your {} limit used{when}", w.pct, w.label));
                    }
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
}
