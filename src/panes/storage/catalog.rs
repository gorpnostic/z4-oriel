//! "get apps": a hand-picked catalog of popular apps with the right package id for each OS, how to pick the
//! install source on this machine, and a one-shot "what's already installed" reader.
//!
//! To add an app, add one `app(...)` line to CATALOG with whichever sources exist. Ids were checked against
//! the real registries on 2026-09-24 (winget show, archlinux.org + AUR RPC, flathub API, Debian madison,
//! npm registry), so keep that habit: don't guess an id.
//!
//! Source order per OS (first one this machine can use wins):
//!   Windows        winget, npm, website
//!   Arch/Omarchy   pacman, AUR (only with yay/paru), npm, flatpak, website
//!   Debian/Ubuntu  apt, npm, flatpak, website
//!   other Linux    npm, flatpak, website

use super::scan::Cmd;
use super::sys::run;
use crate::config::which;
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cat {
    Creative,
    Gaming,
    Network,
    Ai,
    Dev,
    Browsers,
    Media,
    Utilities,
}

/// Display order of the categories (index 0 in the UI is "all").
pub const CATS: [Cat; 8] = [Cat::Creative, Cat::Gaming, Cat::Network, Cat::Ai, Cat::Dev, Cat::Browsers, Cat::Media, Cat::Utilities];

impl Cat {
    pub fn label(self) -> &'static str {
        match self {
            Cat::Creative => "creative",
            Cat::Gaming => "gaming",
            Cat::Network => "vpn & network",
            Cat::Ai => "ai",
            Cat::Dev => "dev",
            Cat::Browsers => "browsers",
            Cat::Media => "chat & media",
            Cat::Utilities => "utilities",
        }
    }
}

/// One catalog app. Empty strings mean "no package there". pacman/apt may list several packages separated by
/// spaces ("nodejs npm"); the first one is what the installed check looks for.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub name: &'static str,
    pub desc: &'static str,
    pub cat: Cat,
    pub home: &'static str,
    pub winget: &'static str,
    pub pacman: &'static str,
    pub aur: &'static str,
    pub apt: &'static str,
    pub flatpak: &'static str,
    pub npm: &'static str,
    /// no package on this OS? offer the download page instead of hiding it
    pub web: bool,
}

const fn app(name: &'static str, cat: Cat, desc: &'static str, home: &'static str) -> Entry {
    Entry { name, desc, cat, home, winget: "", pacman: "", aur: "", apt: "", flatpak: "", npm: "", web: false }
}

impl Entry {
    const fn winget(self, id: &'static str) -> Self {
        Entry { winget: id, ..self }
    }
    const fn pacman(self, p: &'static str) -> Self {
        Entry { pacman: p, ..self }
    }
    const fn aur(self, p: &'static str) -> Self {
        Entry { aur: p, ..self }
    }
    const fn apt(self, p: &'static str) -> Self {
        Entry { apt: p, ..self }
    }
    const fn flatpak(self, id: &'static str) -> Self {
        Entry { flatpak: id, ..self }
    }
    const fn npm(self, p: &'static str) -> Self {
        Entry { npm: p, ..self }
    }
    const fn web(self) -> Self {
        Entry { web: true, ..self }
    }
}

use Cat::*;

#[rustfmt::skip]
pub static CATALOG: &[Entry] = &[
    // creative
    app("OBS Studio", Creative, "record and stream your screen", "https://obsproject.com")
        .winget("OBSProject.OBSStudio").pacman("obs-studio").apt("obs-studio").flatpak("com.obsproject.Studio"),
    app("Blender", Creative, "3D modelling, animation and rendering", "https://www.blender.org")
        .winget("BlenderFoundation.Blender").pacman("blender").apt("blender").flatpak("org.blender.Blender"),
    app("GIMP", Creative, "photo editing and retouching", "https://www.gimp.org")
        .winget("GIMP.GIMP.3").pacman("gimp").apt("gimp").flatpak("org.gimp.GIMP"),
    app("Krita", Creative, "digital painting and illustration", "https://krita.org")
        .winget("KDE.Krita").pacman("krita").apt("krita").flatpak("org.kde.krita"),
    app("Inkscape", Creative, "vector graphics (SVG) editor", "https://inkscape.org")
        .winget("Inkscape.Inkscape").pacman("inkscape").apt("inkscape").flatpak("org.inkscape.Inkscape"),
    app("Audacity", Creative, "record and edit audio", "https://www.audacityteam.org")
        .winget("Audacity.Audacity").pacman("audacity").apt("audacity").flatpak("org.audacityteam.Audacity"),
    app("Kdenlive", Creative, "open-source video editor", "https://kdenlive.org")
        .winget("KDE.Kdenlive").pacman("kdenlive").apt("kdenlive").flatpak("org.kde.kdenlive"),
    app("DaVinci Resolve", Creative, "pro video editing and colour grading", "https://www.blackmagicdesign.com/products/davinciresolve")
        .web(),
    app("HandBrake", Creative, "convert and compress video", "https://handbrake.fr")
        .winget("HandBrake.HandBrake").pacman("handbrake").apt("handbrake").flatpak("fr.handbrake.ghb"),
    // gaming
    app("Steam", Gaming, "Valve's game store and launcher", "https://store.steampowered.com")
        .winget("Valve.Steam").pacman("steam").flatpak("com.valvesoftware.Steam"),
    app("Discord", Gaming, "voice, video and text chat", "https://discord.com")
        .winget("Discord.Discord").pacman("discord").flatpak("com.discordapp.Discord"),
    app("Prism Launcher", Gaming, "Minecraft launcher with mod packs and instances", "https://prismlauncher.org")
        .winget("PrismLauncher.PrismLauncher").pacman("prismlauncher").flatpak("org.prismlauncher.PrismLauncher"),
    app("Minecraft Launcher", Gaming, "the official Minecraft launcher", "https://www.minecraft.net")
        .winget("Mojang.MinecraftLauncher").aur("minecraft-launcher").flatpak("com.mojang.Minecraft"),
    app("Modrinth App", Gaming, "Minecraft mods and modpacks from Modrinth", "https://modrinth.com/app")
        .winget("Modrinth.ModrinthApp").aur("modrinth-app-bin").flatpak("com.modrinth.ModrinthApp"),
    app("CurseForge", Gaming, "mod manager for Minecraft, WoW and more", "https://www.curseforge.com")
        .winget("Overwolf.CurseForge").aur("curseforge"),
    app("Heroic", Gaming, "Epic, GOG and Amazon games launcher", "https://heroicgameslauncher.com")
        .winget("HeroicGamesLauncher.HeroicGamesLauncher").aur("heroic-games-launcher-bin").flatpak("com.heroicgameslauncher.hgl"),
    app("Lutris", Gaming, "run Windows and emulated games on Linux", "https://lutris.net")
        .pacman("lutris").flatpak("net.lutris.Lutris"),
    app("Epic Games", Gaming, "Epic Games Store launcher", "https://store.epicgames.com")
        .winget("EpicGames.EpicGamesLauncher"),
    app("Playnite", Gaming, "one library for every game launcher", "https://playnite.link")
        .winget("Playnite.Playnite"),
    app("RetroArch", Gaming, "emulator frontend for retro consoles", "https://www.retroarch.com")
        .winget("Libretro.RetroArch").pacman("retroarch").apt("retroarch").flatpak("org.libretro.RetroArch"),
    // vpn & network
    app("Tailscale", Network, "private mesh VPN between your devices", "https://tailscale.com/download")
        .winget("Tailscale.Tailscale").pacman("tailscale").web(),
    app("Proton VPN", Network, "free, no-logs VPN from Proton", "https://protonvpn.com")
        .winget("Proton.ProtonVPN").pacman("proton-vpn-gtk-app").flatpak("com.protonvpn.www"),
    app("Mullvad VPN", Network, "privacy-first paid VPN", "https://mullvad.net/download")
        .winget("MullvadVPN.MullvadVPN").aur("mullvad-vpn-bin").web(),
    app("WireGuard", Network, "fast, modern VPN tunnels", "https://www.wireguard.com")
        .winget("WireGuard.WireGuard").pacman("wireguard-tools").apt("wireguard"),
    app("NordVPN", Network, "commercial VPN service", "https://nordvpn.com/download")
        .winget("NordSecurity.NordVPN").aur("nordvpn-bin").web(),
    app("ZeroTier", Network, "virtual LAN across the internet", "https://www.zerotier.com/download")
        .winget("ZeroTier.ZeroTierOne").pacman("zerotier-one").web(),
    // ai
    app("Ollama", Ai, "run open LLMs locally", "https://ollama.com/download")
        .winget("Ollama.Ollama").pacman("ollama").web(),
    app("LM Studio", Ai, "download and chat with local models", "https://lmstudio.ai")
        .winget("ElementLabs.LMStudio").aur("lmstudio-bin").web(),
    app("Claude", Ai, "Anthropic's Claude desktop app", "https://claude.ai/download")
        .winget("Anthropic.Claude"),
    app("ChatGPT", Ai, "OpenAI's ChatGPT desktop app", "https://chatgpt.com/download")
        .winget("9PLM9XGG6VKS"),
    app("Claude Code", Ai, "Claude in your terminal (npm)", "https://claude.com/claude-code")
        .npm("@anthropic-ai/claude-code"),
    app("Codex", Ai, "OpenAI's coding agent CLI (npm)", "https://github.com/openai/codex")
        .npm("@openai/codex"),
    app("Jan", Ai, "offline ChatGPT alternative", "https://jan.ai")
        .winget("Jan.Jan").aur("jan-bin").flatpak("ai.jan.Jan"),
    // dev
    app("VS Code", Dev, "Microsoft's code editor", "https://code.visualstudio.com")
        .winget("Microsoft.VisualStudioCode").aur("visual-studio-code-bin").flatpak("com.visualstudio.code"),
    app("Git", Dev, "version control", "https://git-scm.com")
        .winget("Git.Git").pacman("git").apt("git"),
    app("Node.js LTS", Dev, "JavaScript runtime + npm", "https://nodejs.org")
        .winget("OpenJS.NodeJS.LTS").pacman("nodejs npm").apt("nodejs npm"),
    app("Python", Dev, "Python 3 interpreter", "https://www.python.org")
        .winget("Python.Python.3.13").pacman("python").apt("python3"),
    app("Rust", Dev, "rustup: the Rust toolchain installer", "https://rustup.rs")
        .winget("Rustlang.Rustup").pacman("rustup").apt("rustup"),
    app("Docker", Dev, "containers (Docker Desktop on Windows)", "https://www.docker.com")
        .winget("Docker.DockerDesktop").pacman("docker").apt("docker.io"),
    app("Zed", Dev, "fast, collaborative code editor", "https://zed.dev")
        .winget("ZedIndustries.Zed").pacman("zed").flatpak("dev.zed.Zed"),
    app("JetBrains Toolbox", Dev, "installs and updates JetBrains IDEs", "https://www.jetbrains.com/toolbox-app")
        .winget("JetBrains.Toolbox").aur("jetbrains-toolbox").web(),
    app("GitHub CLI", Dev, "gh: GitHub from the terminal", "https://cli.github.com")
        .winget("GitHub.cli").pacman("github-cli").apt("gh"),
    // browsers
    app("Firefox", Browsers, "Mozilla's browser", "https://www.firefox.com")
        .winget("Mozilla.Firefox").pacman("firefox").flatpak("org.mozilla.firefox"),
    app("Chrome", Browsers, "Google's browser", "https://www.google.com/chrome")
        .winget("Google.Chrome").aur("google-chrome").flatpak("com.google.Chrome"),
    app("Brave", Browsers, "privacy browser with a built-in ad blocker", "https://brave.com")
        .winget("Brave.Brave").aur("brave-bin").flatpak("com.brave.Browser"),
    app("Zen", Browsers, "calm Firefox-based browser", "https://zen-browser.app")
        .winget("Zen-Team.Zen-Browser").aur("zen-browser-bin").flatpak("app.zen_browser.zen"),
    app("Vivaldi", Browsers, "power-user browser with tiling tabs", "https://vivaldi.com")
        .winget("Vivaldi.Vivaldi").pacman("vivaldi").flatpak("com.vivaldi.Vivaldi"),
    // chat & media
    app("Spotify", Media, "music streaming", "https://www.spotify.com/download")
        .winget("Spotify.Spotify").aur("spotify").flatpak("com.spotify.Client"),
    app("VLC", Media, "plays every video and audio format", "https://www.videolan.org/vlc")
        .winget("VideoLAN.VLC").pacman("vlc").apt("vlc").flatpak("org.videolan.VLC"),
    app("mpv", Media, "minimal keyboard-driven video player", "https://mpv.io")
        .winget("shinchiro.mpv").pacman("mpv").apt("mpv").flatpak("io.mpv.Mpv"),
    app("Telegram", Media, "cloud messenger", "https://desktop.telegram.org")
        .winget("Telegram.TelegramDesktop").pacman("telegram-desktop").apt("telegram-desktop").flatpak("org.telegram.desktop"),
    app("Signal", Media, "end-to-end encrypted messenger", "https://signal.org/download")
        .winget("OpenWhisperSystems.Signal").pacman("signal-desktop").flatpak("org.signal.Signal"),
    app("Slack", Media, "team chat", "https://slack.com/downloads")
        .winget("SlackTechnologies.Slack").aur("slack-desktop").flatpak("com.slack.Slack"),
    app("Zoom", Media, "video meetings", "https://zoom.us/download")
        .winget("Zoom.Zoom").aur("zoom").flatpak("us.zoom.Zoom"),
    app("qBittorrent", Media, "ad-free torrent client", "https://www.qbittorrent.org")
        .winget("qBittorrent.qBittorrent").pacman("qbittorrent").apt("qbittorrent").flatpak("org.qbittorrent.qBittorrent"),
    // utilities
    app("7-Zip", Utilities, "open and make zip, 7z, rar archives", "https://www.7-zip.org")
        .winget("7zip.7zip").pacman("7zip").apt("7zip"),
    app("PowerToys", Utilities, "Microsoft's power-user toolbox", "https://learn.microsoft.com/windows/powertoys")
        .winget("Microsoft.PowerToys"),
    app("Everything", Utilities, "instant file-name search", "https://www.voidtools.com")
        .winget("voidtools.Everything"),
    app("ShareX", Utilities, "screenshots, recordings and uploads", "https://getsharex.com")
        .winget("ShareX.ShareX"),
    app("Windows Terminal", Utilities, "Microsoft's tabbed terminal", "https://aka.ms/terminal")
        .winget("Microsoft.WindowsTerminal"),
    app("btop", Utilities, "pretty resource monitor", "https://github.com/aristocratos/btop")
        .pacman("btop").apt("btop"),
    app("fastfetch", Utilities, "system info with a logo", "https://github.com/fastfetch-cli/fastfetch")
        .winget("Fastfetch-cli.Fastfetch").pacman("fastfetch").apt("fastfetch"),
    app("yazi", Utilities, "blazing-fast terminal file manager", "https://yazi-rs.github.io")
        .winget("sxyazi.yazi").pacman("yazi"),
    app("lazygit", Utilities, "git TUI", "https://github.com/jesseduffield/lazygit")
        .winget("JesseDuffield.lazygit").pacman("lazygit").apt("lazygit"),
];

// ------------------------------------------------------------------ this machine

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Windows,
    Arch,
    Debian,
    /// Fedora, openSUSE, …: flatpak and npm only
    Linux,
}

impl Os {
    pub fn label(self) -> &'static str {
        match self {
            Os::Windows => "Windows",
            Os::Arch => "Arch",
            Os::Debian => "Debian/Ubuntu",
            Os::Linux => "this Linux",
        }
    }
}

/// What this machine can install with. Built once (a PATH lookup and a read of /etc/os-release).
#[derive(Clone, Debug)]
pub struct Env {
    pub os: Os,
    /// yay or paru
    pub aur_helper: Option<&'static str>,
    pub flatpak: bool,
    pub npm: bool,
}

impl Env {
    pub fn detect() -> Env {
        let os = if cfg!(windows) {
            Os::Windows
        } else {
            let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
            match distro(&text) {
                Os::Linux if which("pacman").is_some() => Os::Arch,
                Os::Linux if which("apt").is_some() => Os::Debian,
                os => os,
            }
        };
        Env {
            os,
            aur_helper: ["yay", "paru"].into_iter().find(|h| os == Os::Arch && which(h).is_some()),
            flatpak: os != Os::Windows && which("flatpak").is_some(),
            npm: which("npm").is_some(),
        }
    }
}

/// /etc/os-release -> the distro family, from ID and ID_LIKE (Omarchy, Manjaro, CachyOS … say ID_LIKE=arch).
pub fn distro(os_release: &str) -> Os {
    let mut words = vec![];
    for l in os_release.lines() {
        if let Some((k, v)) = l.split_once('=') {
            if k == "ID" || k == "ID_LIKE" {
                words.extend(v.trim().trim_matches('"').split_whitespace().map(str::to_lowercase));
            }
        }
    }
    let has = |names: &[&str]| words.iter().any(|w| names.contains(&w.as_str()));
    if has(&["arch", "archarm", "manjaro", "endeavouros", "cachyos", "omarchy", "garuda"]) {
        Os::Arch
    } else if has(&["debian", "ubuntu", "linuxmint", "pop", "raspbian"]) {
        Os::Debian
    } else {
        Os::Linux
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Src {
    Winget,
    Pacman,
    Aur,
    Apt,
    Flatpak,
    Npm,
    Web,
}

impl Src {
    pub fn label(self) -> &'static str {
        match self {
            Src::Winget => "winget",
            Src::Pacman => "pacman",
            Src::Aur => "aur",
            Src::Apt => "apt",
            Src::Flatpak => "flatpak",
            Src::Npm => "npm",
            Src::Web => "website",
        }
    }
}

/// How an entry gets installed here. `install` is None for a website, or when the tool it needs is missing
/// (then `missing` says what to get).
#[derive(Clone, Debug)]
pub struct Pick {
    pub src: Src,
    pub install: Option<Cmd>,
    pub missing: Option<String>,
}

fn cmd(prog: &str, pre: &[&str], pkgs: &str, post: &[&str]) -> Cmd {
    let mut args: Vec<&str> = pre.to_vec();
    args.extend(pkgs.split_whitespace());
    args.extend(post);
    Cmd::new(prog, &args)
}

pub fn install_cmd(src: Src, id: &str, env: &Env) -> Option<Cmd> {
    Some(match src {
        Src::Winget => cmd("winget", &["install", "--id"], id, &["-e", "--accept-package-agreements", "--accept-source-agreements"]),
        Src::Pacman => cmd("sudo", &["pacman", "-S", "--needed"], id, &[]),
        Src::Aur => cmd(env.aur_helper?, &["-S"], id, &[]),
        Src::Apt => cmd("sudo", &["apt", "install"], id, &[]),
        Src::Flatpak => cmd("flatpak", &["install", "-y", "flathub"], id, &[]),
        // npm is npm.cmd on Windows: run it through cmd so the terminal pane can start it
        Src::Npm if cfg!(windows) => cmd("cmd", &["/c", "npm", "i", "-g"], id, &[]),
        Src::Npm => cmd("npm", &["i", "-g"], id, &[]),
        Src::Web => return None,
    })
}

/// The source this machine should use for `e`, or None if the app isn't offered on this OS at all (hidden).
pub fn pick(e: &Entry, env: &Env) -> Option<Pick> {
    let ok = |src: Src, id: &str| Some(Pick { src, install: install_cmd(src, id, env), missing: None });
    let need = |src: Src, what: &str| Some(Pick { src, install: None, missing: Some(what.into()) });
    let has = |s: &str| !s.is_empty();
    let web = || e.web.then(|| Pick { src: Src::Web, install: None, missing: None });
    match env.os {
        Os::Windows if has(e.winget) => ok(Src::Winget, e.winget),
        Os::Arch if has(e.pacman) => ok(Src::Pacman, e.pacman),
        Os::Arch if has(e.aur) && env.aur_helper.is_some() => ok(Src::Aur, e.aur),
        Os::Debian if has(e.apt) => ok(Src::Apt, e.apt),
        _ if has(e.npm) && env.npm => ok(Src::Npm, e.npm),
        Os::Windows => None,
        _ if has(e.flatpak) && env.flatpak => ok(Src::Flatpak, e.flatpak),
        _ => None,
    }
    .or_else(web)
    .or_else(|| {
        if has(e.npm) {
            need(Src::Npm, "needs npm (get Node.js first)")
        } else if env.os == Os::Arch && has(e.aur) {
            need(Src::Aur, "needs yay or paru (AUR helper)")
        } else if env.os != Os::Windows && has(e.flatpak) {
            need(Src::Flatpak, "needs flatpak")
        } else {
            None
        }
    })
}

// ------------------------------------------------------------------ what's installed

/// Package ids already on this machine, keyed "winget:<id>" / "pkg:<name>" (pacman or dpkg) / "flatpak:<id>" /
/// "npm:<name>", all lowercase, plus the names of Windows apps winget couldn't match to an id.
#[derive(Clone, Debug, Default)]
pub struct Installed {
    pub ids: HashSet<String>,
    pub names: Vec<String>,
}

impl Installed {
    pub fn has(&self, e: &Entry) -> bool {
        let first = |s: &str| s.split_whitespace().next().unwrap_or("").to_lowercase();
        let keys = [
            ("winget", e.winget.to_lowercase()),
            ("pkg", first(e.pacman)),
            ("pkg", first(e.aur)),
            ("pkg", first(e.apt)),
            ("flatpak", e.flatpak.to_lowercase()),
            ("npm", e.npm.to_lowercase()),
        ];
        keys.iter().any(|(k, v)| !v.is_empty() && self.ids.contains(&format!("{k}:{v}")))
            || (!e.winget.is_empty() && self.names.iter().any(|n| word_match(n, &e.name.to_lowercase())))
    }
}

/// `needle` appears in `hay` as whole words ("git" in "git 2.4" but not in "github desktop").
fn word_match(hay: &str, needle: &str) -> bool {
    let alnum = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric());
    hay.match_indices(needle).any(|(i, m)| !alnum(hay[..i].chars().next_back()) && !alnum(hay[i + m.len()..].chars().next()))
}

/// Read everything that's installed, in one pass per package manager. Slow (winget list takes seconds) — call
/// it on a background thread.
pub fn installed(env: &Env) -> Installed {
    let mut inst = Installed::default();
    let mut add = |k: &str, v: &str| {
        inst.ids.insert(format!("{k}:{}", v.trim().to_lowercase()));
    };
    if env.os == Os::Windows {
        if which("winget").is_some() {
            let text = run("winget", &["list", "--accept-source-agreements", "--disable-interactivity"]).unwrap_or_default();
            let (ids, names) = parse_winget_list(&text);
            ids.iter().for_each(|i| add("winget", i));
            inst.names = names;
        }
    } else {
        if which("pacman").is_some() {
            run("pacman", &["-Qq"]).unwrap_or_default().lines().for_each(|l| add("pkg", l));
        }
        if which("dpkg-query").is_some() {
            let out = run("dpkg-query", &["-W", "-f=${db:Status-Abbrev}\t${Package}\n"]).unwrap_or_default();
            out.lines().filter(|l| l.starts_with("ii")).filter_map(|l| l.split('\t').nth(1)).for_each(|p| add("pkg", p));
        }
        if env.flatpak {
            run("flatpak", &["list", "--app", "--columns=application"]).unwrap_or_default().lines().for_each(|l| add("flatpak", l));
        }
    }
    if env.npm {
        let out = if cfg!(windows) {
            run("cmd", &["/c", "npm", "ls", "-g", "--depth=0", "--parseable"])
        } else {
            run("npm", &["ls", "-g", "--depth=0", "--parseable"])
        };
        parse_npm_ls(&out.unwrap_or_default()).iter().for_each(|p| add("npm", p));
    }
    inst
}

/// `winget list` table -> (winget ids, names of apps that only have an ARP/MSIX id). Columns come from the
/// header's character positions, like the search parser in sys.rs.
pub fn parse_winget_list(text: &str) -> (Vec<String>, Vec<String>) {
    let lines: Vec<Vec<char>> = text.lines().map(|l| l.rsplit('\r').next().unwrap_or("").trim_end().chars().collect()).collect();
    let Some(head) = lines.iter().position(|l| {
        let s: String = l.iter().collect();
        s.trim_start().starts_with("Name") && s.contains(" Id ")
    }) else {
        return (vec![], vec![]);
    };
    let h: String = lines[head].iter().collect();
    let off = h.chars().count() - h.trim_start().chars().count();
    let col = |name: &str| h.find(&format!(" {name} ")).map(|i| h[..i + 1].chars().count());
    let (Some(id), Some(ver)) = (col("Id"), col("Version")) else { return (vec![], vec![]) };
    let cut = |l: &[char], a: usize, b: usize| -> String { if a >= l.len().min(b) { String::new() } else { l[a..b.min(l.len())].iter().collect::<String>().trim().to_string() } };
    let (mut ids, mut names) = (vec![], vec![]);
    for l in &lines[head + 1..] {
        if l.is_empty() || l.iter().all(|c| *c == '-' || *c == '─') {
            continue;
        }
        let wid = cut(l, id, ver);
        let name = cut(l, off, id).to_lowercase();
        if wid.is_empty() {
            continue;
        }
        if wid.starts_with("ARP\\") || wid.starts_with("MSIX\\") {
            names.push(name);
        } else {
            ids.push(wid.trim_end_matches('…').to_string());
        }
    }
    (ids, names)
}

/// `npm ls -g --depth=0 --parseable` -> package names (the first line is the global folder itself).
pub fn parse_npm_ls(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim().replace('\\', "/");
            let (_, rest) = l.rsplit_once("node_modules/")?;
            (!rest.is_empty()).then(|| rest.to_string())
        })
        .collect()
}
