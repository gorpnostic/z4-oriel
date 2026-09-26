//! The event center: things worth knowing that happened while you were busy (an agent finished or needs you,
//! an approval is waiting, a build failed, a plan is about to start, an install finished, memory is running out,
//! an AI's usage is near its limit, a new version is out). Each one is a toast, a line in the alerts app, and,
//! when the terminal isn't the window you're in, a desktop notification.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    /// Worth a desktop notification when you're in another window.
    pub fn loud(self) -> bool {
        !matches!(self, Kind::Update)
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
/// until it clears (or the usage window resets).
pub fn watch(send: impl Fn(Kind, String) + Send + 'static) {
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
            let pct = used as f64 / total as f64;
            if pct > 0.92 && !mem_warned {
                mem_warned = true;
                send(Kind::Memory, format!("memory is {:.0}% full ({} of {}): the system app shows what's using it", pct * 100.0, crate::ui::human_bytes(used), crate::ui::human_bytes(total)));
            } else if pct < 0.85 {
                mem_warned = false;
            }
            if tick % 10 == 0 {
                let limits = crate::panes::agents::usage_limits();
                for (agent, w) in limits {
                    let warn = if w.label == "weekly" { 90.0 } else { 85.0 };
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
