//! Small helpers shared by the "your AIs" modules: paths (overridable for tests), dates without a date crate,
//! number formatting, hidden child processes with a timeout, atomic writes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Every place the app reads or writes. Tests point these at temp dirs so the real ~/.claude and ~/.codex are
/// never touched.
#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    /// Claude Code's config dir (`CLAUDE_CONFIG_DIR` or ~/.claude)
    pub claude: PathBuf,
    /// ~/.claude.json (MCP servers live here)
    pub claude_json: PathBuf,
    /// `CODEX_HOME` or ~/.codex
    pub codex: PathBuf,
    /// oriel's data dir (usage cache, usage-sink output)
    pub data: PathBuf,
    /// Ollama's API base
    pub ollama: String,
}

impl Paths {
    pub fn real(cfg: &crate::config::Config) -> Paths {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
        let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
        Paths { claude_json: home.join(".claude.json"), home, claude, codex, data: crate::config::data_dir(), ollama: cfg.ai.ollama_url.clone() }
    }

    /// Everything under one folder (tests).
    #[cfg(test)]
    pub fn under(root: &Path) -> Paths {
        let home = root.join("home");
        Paths {
            claude: home.join(".claude"),
            claude_json: home.join(".claude.json"),
            codex: home.join(".codex"),
            data: root.join("data"),
            home,
            ollama: "http://127.0.0.1:1".into(), // nothing listens on port 1
        }
    }

    pub fn sink_file(&self) -> PathBuf {
        self.data.join("usage").join("claude.json")
    }
    pub fn cache_file(&self) -> PathBuf {
        self.data.join("usage-cache.json")
    }
    pub fn claude_settings(&self) -> PathBuf {
        self.claude.join("settings.json")
    }
    pub fn codex_config(&self) -> PathBuf {
        self.codex.join("config.toml")
    }
}

// ------------------------------------------------------------------ time

pub fn now() -> i64 {
    crate::panes::files::clock::now_secs()
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// (year, month, day) for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// "2026-09-25T05:19:50.077Z" (or with a +hh:mm offset) → epoch seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)? as u32, num(8..10)? as u32);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut t = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se;
    // timezone suffix after optional fraction
    let rest = &s[19..];
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let oh: i64 = rest.get(1..3).and_then(|x| x.parse().ok()).unwrap_or(0);
        let om: i64 = rest.get(4..6).and_then(|x| x.parse().ok()).unwrap_or(0);
        let off = oh * 3600 + om * 60;
        t -= if sign == '+' { off } else { -off };
    }
    Some(t)
}

/// This machine's current UTC offset in seconds (asks the OS through files::clock).
pub fn local_offset() -> i64 {
    let n = now();
    let l = crate::panes::files::clock::local(n);
    let as_utc = days_from_civil(l.year as i64, l.month, l.day) * 86400 + l.hour as i64 * 3600 + l.min as i64 * 60 + l.sec as i64;
    // round to the quarter hour: clock::local has second precision, the offset never has odd seconds
    ((as_utc - n) as f64 / 900.0).round() as i64 * 900
}

/// Local day number for an epoch second.
pub fn day_of(t: i64, off: i64) -> i64 {
    (t + off).div_euclid(86400)
}

const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]; // 1970-01-01 was a Thursday

/// "Thu Sep 24"
pub fn day_label(day: i64) -> String {
    let (_, m, d) = civil_from_days(day);
    format!("{} {} {:>2}", WD[day.rem_euclid(7) as usize], MON[(m - 1) as usize], d)
}

/// "14:00" for an epoch second in local time.
pub fn hhmm(t: i64, off: i64) -> String {
    let s = (t + off).rem_euclid(86400);
    format!("{:02}:{:02}", s / 3600, (s / 60) % 60)
}

/// "2h 14m", "4d 3h", "12m", "40s".
pub fn dur(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 86400 {
        format!("{}d {}h", s / 86400, (s % 86400) / 3600)
    } else if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// "3m ago", "2h ago", "just now".
pub fn ago(secs: i64) -> String {
    if secs < 45 { "just now".into() } else { format!("{} ago", dur(secs)) }
}

// ------------------------------------------------------------------ numbers

/// 1234 → "1.2K", 3_400_000 → "3.4M".
pub fn tok(n: u64) -> String {
    let v = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.2}B", v / 1e9)
    } else if n >= 10_000_000 {
        format!("{:.0}M", v / 1e6)
    } else if n >= 1_000_000 {
        format!("{:.1}M", v / 1e6)
    } else if n >= 10_000 {
        format!("{:.0}K", v / 1e3)
    } else if n >= 1_000 {
        format!("{:.1}K", v / 1e3)
    } else {
        n.to_string()
    }
}

pub fn money(v: f64) -> String {
    if v >= 1000.0 {
        format!("${:.0}", v)
    } else if v >= 100.0 {
        format!("${v:.1}")
    } else {
        format!("${v:.2}")
    }
}

/// ▁▂▃▄▅▆▇█ for a series, scaled to its max.
pub fn spark(v: &[u64]) -> String {
    const B: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = v.iter().copied().max().unwrap_or(0).max(1);
    v.iter().map(|&x| if x == 0 { ' ' } else { B[((x as f64 / max as f64) * 7.0).round().clamp(0.0, 7.0) as usize] }).collect()
}

// ------------------------------------------------------------------ files

/// Write via a temp file + rename, so a reader never sees half a file.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".oriel-tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

// ------------------------------------------------------------------ processes

/// Run a program hidden (CREATE_NO_WINDOW on Windows), give up after `limit`, return stdout+stderr.
pub fn run_timeout(prog: &Path, args: &[&str], limit: Duration) -> Option<String> {
    use std::io::Read;
    let mut c = Command::new(prog);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).env("NO_COLOR", "1").env("CI", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let mut child = c.spawn().ok()?;
    let mut out = child.stdout.take()?;
    let mut err = child.stderr.take()?;
    let ro = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let re = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if t0.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut s = ro.join().unwrap_or_default();
    s.push('\n');
    s.push_str(&re.join().unwrap_or_default());
    Some(s)
}

/// First "1.2.3"-looking token in a version string.
pub fn find_version(s: &str) -> Option<String> {
    for w in s.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')') {
        let w = w.trim_start_matches('v');
        let mut parts = w.split('.');
        let ok = parts.next().is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            && parts.next().is_some_and(|p| !p.is_empty() && p.chars().take_while(|c| c.is_ascii_digit()).count() > 0);
        if ok {
            return Some(w.trim_end_matches(|c: char| !c.is_ascii_alphanumeric()).to_string());
        }
    }
    None
}

/// The shell that hosts an install / sign-in command in a terminal pane. When the command ends it writes the exit
/// code to `marker` (the "your AIs" app watches for it: alert + recheck), says how it went, and pauses so the
/// output stays readable (a finished terminal pane closes itself); then it exits with the command's own code, not
/// the pause's.
pub fn host_command(cmdline: &str, marker: &Path) -> (String, Vec<String>) {
    let m = marker.to_string_lossy();
    if cfg!(windows) {
        let prog = crate::config::which("pwsh").map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "powershell.exe".into());
        let script = [
            // what's been installed since oriel started (Node, say) is on the machine's PATH, not in the one we inherited
            "$env:Path = (($env:Path -split ';') + ([Environment]::GetEnvironmentVariable('Path', 'Machine') -split ';') + ([Environment]::GetEnvironmentVariable('Path', 'User') -split ';') | Where-Object { $_ } | Select-Object -Unique) -join ';'".to_string(),
            // (no ErrorActionPreference = Stop: an installer script that shrugs off an error of its own should still finish)
            "$global:LASTEXITCODE = 0; $ok = $true; $c = $null".to_string(),
            // `finally`, because an `irm … | iex` script may `exit` (or you press ctrl+c): its code is unknown then,
            // so the marker says "ended" and the row's recheck tells how it went
            format!(
                "try {{ try {{ {cmdline}; $ok = $? }} catch {{ Write-Host $_ -ForegroundColor Red; $ok = $false }}; $c = if ($LASTEXITCODE) {{ $LASTEXITCODE }} elseif ($ok) {{ 0 }} else {{ 1 }} }} finally {{ \
                 Set-Content -LiteralPath '{}' -Value $(if ($null -eq $c) {{ 'ended' }} else {{ $c }}) -ErrorAction SilentlyContinue; Write-Host ''; \
                 if ($null -eq $c) {{ Write-Host 'ended' }} elseif ($c -eq 0) {{ Write-Host 'finished' -ForegroundColor Green }} else {{ Write-Host \"failed (exit $c)\" -ForegroundColor Red }}; \
                 $null = Read-Host 'press enter to close' }}",
                m.replace('\'', "''")
            ),
            "exit $c".to_string(),
        ]
        .join("; ");
        (prog, vec!["-NoProfile".into(), "-ExecutionPolicy".into(), "Bypass".into(), "-Command".into(), script])
    } else {
        let script = format!(
            "{cmdline}; c=$?; printf '%s' \"$c\" > {}; if [ \"$c\" -eq 0 ]; then printf '\\nfinished'; else printf '\\nfailed (exit %s)' \"$c\"; fi; printf ' - press enter to close '; read _; exit $c",
            super::saver::sh_quote(&m)
        );
        ("sh".into(), vec!["-c".into(), script])
    }
}

/// PATH as a new terminal would see it now. On Windows that's this process's PATH plus the machine's and the
/// user's from the registry (an install since oriel started changes those, not ours); elsewhere it's ours.
pub fn fresh_path() -> std::ffi::OsString {
    let mine = std::env::var_os("PATH").unwrap_or_default();
    if !cfg!(windows) {
        return mine;
    }
    let ps = Path::new("powershell.exe");
    let script = "[Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')";
    match run_timeout(ps, &["-NoProfile", "-NonInteractive", "-Command", script], Duration::from_secs(5)) {
        Some(out) => add_dirs(mine, &out),
        None => mine,
    }
}

/// `mine` plus the folders in `out`, PowerShell's answer: the registry's PATH on its first line (stdout), then
/// whatever it printed on stderr (Windows PowerShell started from pwsh 7 can complain about modules).
fn add_dirs(mine: std::ffi::OsString, out: &str) -> std::ffi::OsString {
    let reg = out.lines().next().unwrap_or("");
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&mine).collect();
    for d in reg.split(';').map(str::trim).filter(|d| !d.is_empty()) {
        let d = PathBuf::from(d);
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs).unwrap_or(mine)
}

/// `config::which`, against a given PATH.
pub fn which_in(prog: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) && !prog.contains('.') { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::split_paths(path).flat_map(|d| exts.iter().map(move |e| d.join(format!("{prog}{e}")))).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ais_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2026, 9, 25)), (2026, 9, 25));
        assert_eq!(parse_iso("1970-01-02T00:00:01.5Z"), Some(86401));
        assert_eq!(parse_iso("1970-01-01T02:00:00+02:00"), Some(0));
        assert_eq!(day_label(days_from_civil(2026, 9, 24)), "Thu Sep 24");
        assert_eq!(dur(2 * 3600 + 14 * 60), "2h 14m");
        assert_eq!(tok(1_234_567), "1.2M");
        assert_eq!(find_version("2.1.282 (Claude Code)").as_deref(), Some("2.1.282"));
        assert_eq!(find_version("codex-cli 0.147.0").as_deref(), Some("0.147.0"));
    }

    #[test]
    fn ais_fresh_path_adds_new_folders_only() {
        let mine = std::env::join_paths(["/a", "/b"]).unwrap();
        let got: Vec<PathBuf> = std::env::split_paths(&add_dirs(mine.clone(), "/b;/c; ;\r\n\nType data error;/d\n")).collect();
        assert_eq!(got, ["/a", "/b", "/c"].map(PathBuf::from), "stderr after the first line is never a folder");
        assert_eq!(add_dirs(mine.clone(), "\nsome error"), mine);
    }
}
