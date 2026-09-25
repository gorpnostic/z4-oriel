//! The AI coding CLI catalog (docs/ai-research.md §1): how to install each one per OS, how to check it's there,
//! how to sign in. Detection runs on background threads (PATH + known folders, `--version` with a 3 s timeout).
//!
//! Install order per OS (first usable wins):
//!   Windows        native installer script / winget, then scoop, then npm
//!   Arch           pacman `extra` (AUR only with yay/paru), then the script, then npm
//!   other Linux    the script, then npm / uv
//! Each command names the tool it needs as its first word ("winget", "npm", "curl" ...); a command whose tool
//! isn't installed is skipped. Script commands on Windows are PowerShell one-liners (`irm … | iex`).

use super::util::{Paths, find_version, run_timeout};
use crate::config::which;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct Cli {
    pub id: &'static str,
    pub name: &'static str,
    pub bin: &'static str,
    pub desc: &'static str,
    pub docs: &'static str,
    pub win: &'static [&'static str],
    pub arch: &'static [&'static str],
    pub unix: &'static [&'static str],
    /// arguments that print the version
    pub ver: &'static [&'static str],
    /// the command that signs in (None = API keys only)
    pub login: Option<&'static str>,
    /// a file (relative to home) whose existence means "signed in" — never read
    pub cred: Option<&'static str>,
    /// an env var that also counts as signed in
    pub key_env: Option<&'static str>,
    /// shown when there's no way to install it here
    pub note: &'static str,
}

const V: &[&str] = &["--version"];

#[rustfmt::skip]
pub static CLIS: &[Cli] = &[
    Cli { id: "claude", name: "Claude Code", bin: "claude", desc: "Anthropic's coding agent", docs: "https://code.claude.com/docs/en/setup",
        win: &["irm https://claude.ai/install.ps1 | iex", "winget install Anthropic.ClaudeCode", "npm install -g @anthropic-ai/claude-code"],
        arch: &[], unix: &["curl -fsSL https://claude.ai/install.sh | bash", "npm install -g @anthropic-ai/claude-code"],
        ver: V, login: Some("claude"), cred: Some(".claude/.credentials.json"), key_env: Some("ANTHROPIC_API_KEY"), note: "" },
    Cli { id: "codex", name: "Codex", bin: "codex", desc: "OpenAI's coding agent", docs: "https://developers.openai.com/codex/cli",
        win: &["irm https://chatgpt.com/codex/install.ps1 | iex", "winget install OpenAI.Codex", "npm install -g @openai/codex"],
        arch: &["sudo pacman -S openai-codex"], unix: &["curl -fsSL https://chatgpt.com/codex/install.sh | sh", "npm install -g @openai/codex"],
        ver: V, login: Some("codex login"), cred: Some(".codex/auth.json"), key_env: Some("OPENAI_API_KEY"), note: "" },
    Cli { id: "kimi", name: "Kimi Code", bin: "kimi", desc: "Moonshot's coding agent (needs Git for Windows)", docs: "https://code.kimi.com",
        win: &["irm https://code.kimi.com/kimi-code/install.ps1 | iex", "npm install -g @moonshot-ai/kimi-code"],
        arch: &[], unix: &["curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash", "npm install -g @moonshot-ai/kimi-code"],
        ver: V, login: Some("kimi login"), cred: Some(".kimi-code/credentials/kimi-code.json"), key_env: None, note: "" },
    Cli { id: "gemini", name: "Gemini CLI", bin: "gemini", desc: "Google's terminal agent", docs: "https://github.com/google-gemini/gemini-cli",
        win: &["npm install -g @google/gemini-cli"], arch: &["sudo pacman -S gemini-cli"], unix: &["npm install -g @google/gemini-cli"],
        ver: V, login: Some("gemini"), cred: Some(".gemini/oauth_creds.json"), key_env: Some("GEMINI_API_KEY"), note: "" },
    Cli { id: "opencode", name: "OpenCode", bin: "opencode", desc: "open-source agent, any provider", docs: "https://opencode.ai/docs",
        win: &["scoop install opencode", "npm install -g opencode-ai"], arch: &["sudo pacman -S opencode"],
        unix: &["curl -fsSL https://opencode.ai/install | bash", "npm install -g opencode-ai"],
        ver: V, login: Some("opencode auth login"), cred: Some(".local/share/opencode/auth.json"), key_env: None, note: "" },
    Cli { id: "aider", name: "Aider", bin: "aider", desc: "pair programming with any model", docs: "https://aider.chat/docs/install.html",
        win: &["irm https://aider.chat/install.ps1 | iex"], arch: &[], unix: &["curl -LsSf https://aider.chat/install.sh | sh", "uv tool install aider-chat"],
        ver: V, login: None, cred: None, key_env: None, note: "uses provider API keys" },
    Cli { id: "copilot", name: "Copilot CLI", bin: "copilot", desc: "GitHub Copilot in the terminal", docs: "https://docs.github.com/copilot/how-tos/set-up/install-copilot-cli",
        win: &["winget install GitHub.Copilot", "npm install -g @github/copilot"], arch: &[], unix: &["curl -fsSL https://gh.io/copilot-install | bash", "npm install -g @github/copilot"],
        ver: &["version"], login: Some("copilot login"), cred: None, key_env: None, note: "" },
    Cli { id: "cursor", name: "Cursor Agent", bin: "agent", desc: "Cursor's agent outside the editor", docs: "https://cursor.com/cli",
        win: &["irm 'https://cursor.com/install?win32=true' | iex"], arch: &[], unix: &["curl https://cursor.com/install -fsS | bash"],
        ver: V, login: Some("agent login"), cred: None, key_env: None, note: "" },
    Cli { id: "qwen", name: "Qwen Code", bin: "qwen", desc: "Alibaba's Gemini-CLI fork", docs: "https://github.com/QwenLM/qwen-code",
        win: &["npm install -g @qwen-code/qwen-code"], arch: &["sudo pacman -S qwen-code"], unix: &["npm install -g @qwen-code/qwen-code"],
        ver: V, login: Some("qwen"), cred: None, key_env: None, note: "" },
    Cli { id: "amp", name: "Amp", bin: "amp", desc: "Sourcegraph's agent", docs: "https://ampcode.com/manual",
        win: &[], arch: &[], unix: &["curl -fsSL https://ampcode.com/install.sh | bash", "npm install -g @ampcode/cli"],
        ver: &["version"], login: Some("amp login"), cred: None, key_env: None, note: "WSL only on Windows" },
    Cli { id: "droid", name: "Factory Droid", bin: "droid", desc: "Factory's agent", docs: "https://docs.factory.ai/cli/getting-started/quickstart",
        win: &["irm https://app.factory.ai/cli/windows | iex"], arch: &[], unix: &["curl -fsSL https://app.factory.ai/cli | sh"],
        ver: V, login: Some("droid"), cred: None, key_env: None, note: "" },
    Cli { id: "crush", name: "Crush", bin: "crush", desc: "Charm's glamorous agent", docs: "https://github.com/charmbracelet/crush",
        win: &["winget install charmbracelet.crush", "npm install -g @charmland/crush"], arch: &["yay -S crush-bin", "paru -S crush-bin"], unix: &["npm install -g @charmland/crush"],
        ver: V, login: None, cred: None, key_env: None, note: "uses provider env vars" },
    Cli { id: "goose", name: "Goose", bin: "goose", desc: "extensible open-source agent", docs: "https://github.com/aaif-goose/goose",
        win: &["irm https://github.com/aaif-goose/goose/releases/download/stable/download_cli.ps1 | iex"], arch: &[],
        unix: &["curl -fsSL https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh | bash"],
        ver: V, login: Some("goose configure"), cred: None, key_env: None, note: "" },
];

#[cfg(test)]
pub fn by_id(id: &str) -> Option<&'static Cli> {
    CLIS.iter().find(|c| c.id == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    Arch,
    Linux,
}

impl Os {
    pub fn detect() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if which("pacman").is_some() {
            Os::Arch
        } else {
            Os::Linux
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Os::Windows => "Windows",
            Os::Arch => "Arch",
            Os::Linux => "Linux",
        }
    }
}

/// The tool a command line needs: "irm" is PowerShell (always there on Windows), "sudo pacman" needs pacman.
fn needs(cmd: &str) -> &str {
    let mut w = cmd.split_whitespace();
    match w.next().unwrap_or("") {
        "sudo" => w.next().unwrap_or(""),
        "irm" => "",
        first => first,
    }
}

/// The install command to offer on this OS: the first one whose tool exists (`has` = is that tool installed).
/// Falls back to the first listed command (so the user sees what it would take) with `ready = false`.
pub fn options(c: &Cli, os: Os) -> Vec<&'static str> {
    match os {
        Os::Windows => c.win.to_vec(),
        Os::Arch => c.arch.iter().chain(c.unix.iter()).copied().collect(),
        Os::Linux => c.unix.to_vec(),
    }
}

pub fn pick(c: &Cli, os: Os, has: &dyn Fn(&str) -> bool) -> Option<(String, bool)> {
    let list = options(c, os);
    for cmd in &list {
        let n = needs(cmd);
        if n.is_empty() || has(n) {
            return Some((cmd.to_string(), true));
        }
    }
    list.first().map(|c| (c.to_string(), false))
}

#[derive(Clone, Debug, Default)]
pub struct Found {
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    /// Some(how) when signed in ("OAuth", "API key", "ChatGPT" ...), Some("") = unknown for this CLI
    pub signed: Option<String>,
}

impl Found {
    pub fn installed(&self) -> bool {
        self.path.is_some()
    }
}

/// Folders (relative to home) installers drop binaries into without putting them on PATH for this process.
const KNOWN: &[&str] = &[".local/bin", ".kimi-code/bin", ".claude/local", ".opencode/bin", ".factory/bin", ".bun/bin", "AppData/Roaming/npm", ".npm-global/bin", ".cargo/bin"];

pub fn locate(bin: &str, home: &std::path::Path) -> Option<PathBuf> {
    if let Some(p) = which(bin) {
        return Some(p);
    }
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat"] } else { &[""] };
    for d in KNOWN {
        for e in exts {
            let p = home.join(d).join(format!("{bin}{e}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Where the binary is, its version, whether it's signed in. Blocking (runs `--version`): background threads only.
pub fn detect(c: &Cli, paths: &Paths) -> Found {
    let Some(path) = locate(c.bin, &paths.home) else { return Found::default() };
    let version = run_timeout(&path, c.ver, Duration::from_secs(3)).and_then(|s| find_version(&s));
    let signed = signed_in(c, paths, Some(&path));
    Found { path: Some(path), version, signed }
}

/// Existence checks only — credential files are never opened.
pub fn signed_in(c: &Cli, paths: &Paths, bin: Option<&PathBuf>) -> Option<String> {
    if c.id == "codex" {
        // `codex login status` knows best ("Logged in using ChatGPT"); the auth.json check is the fallback
        if let Some(out) = bin.and_then(|b| run_timeout(b, &["login", "status"], Duration::from_secs(3))) {
            let l = out.to_lowercase();
            if l.contains("not logged in") {
                return None;
            }
            if l.contains("logged in") {
                return Some(if l.contains("chatgpt") { "ChatGPT".into() } else if l.contains("api key") { "API key".into() } else { "yes".into() });
            }
        }
    }
    if let Some(f) = c.cred {
        let p = if c.id == "claude" { paths.claude.join(".credentials.json") } else if c.id == "codex" { paths.codex.join("auth.json") } else { paths.home.join(f) };
        if p.is_file() {
            return Some(if c.id == "claude" { "OAuth".into() } else { "yes".into() });
        }
    }
    if let Some(k) = c.key_env {
        if std::env::var_os(k).is_some_and(|v| !v.is_empty()) {
            return Some("API key".into());
        }
    }
    if c.cred.is_none() && c.key_env.is_none() {
        return Some(String::new()); // can't tell cheaply
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ais_pick_prefers_native_then_npm() {
        let claude = by_id("claude").unwrap();
        let (cmd, ready) = pick(claude, Os::Windows, &|_| false).unwrap();
        assert!(cmd.starts_with("irm https://claude.ai/install.ps1") && ready);
        // gemini on Windows is npm-only: without npm it's offered but not ready
        let gem = by_id("gemini").unwrap();
        assert_eq!(pick(gem, Os::Windows, &|_| false), Some(("npm install -g @google/gemini-cli".into(), false)));
        assert_eq!(pick(gem, Os::Windows, &|t| t == "npm").unwrap().1, true);
        // Arch: pacman first
        let codex = by_id("codex").unwrap();
        assert_eq!(pick(codex, Os::Arch, &|t| t == "pacman").unwrap().0, "sudo pacman -S openai-codex");
        assert_eq!(pick(codex, Os::Arch, &|t| t == "curl").unwrap().0, "curl -fsSL https://chatgpt.com/codex/install.sh | sh");
        // Amp has nothing for Windows
        assert_eq!(pick(by_id("amp").unwrap(), Os::Windows, &|_| true), None);
        assert_eq!(CLIS.len(), 13);
    }
}
