//! The help app: every key, command and how-to, by topic. Opens on the topic for the app you came from
//! (`?` anywhere that doesn't use the key, F10, or the palette).

use crate::pane::{Cx, Pane};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::cell::RefCell;

thread_local! {
    /// The app the user was in when they asked for help (set by the app just before opening this; the UI is
    /// single-threaded, and thread-local keeps parallel tests apart).
    static CONTEXT: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn set_context(app: &str) {
    CONTEXT.with(|c| *c.borrow_mut() = Some(app.to_string()));
}

enum It {
    H(&'static str),
    P(&'static str),
    K(&'static str, &'static str),
    Gap,
}
use It::*;

struct Topic {
    icon: &'static str,
    title: &'static str,
    /// the app this topic belongs to (help opens here when you come from that app)
    app: &'static str,
    items: Vec<It>,
}

fn topics() -> Vec<Topic> {
    let mut chat_cmds: Vec<It> = vec![P("Type / in the chat box: a menu shows every command, and after a command it lists the choices. ↑↓ pick, tab completes, enter runs."), Gap];
    for (c, a, d) in crate::panes::chat::COMMANDS {
        let left: &'static str = Box::leak(format!("{c} {a}").trim_end().to_string().into_boxed_str());
        chat_cmds.push(K(left, d));
    }
    vec![
        Topic { icon: "home", title: "getting started", app: "home", items: vec![
            H("oriel in one minute"),
            P("The sidebar on the left holds every app. Click one, or press its key. The ai apps (F1–F3) are for talking to and running AIs; the tools (F4–F9) are music, system, files, notes, storage and a real terminal."),
            Gap,
            K("F1  chat", "talk to Claude Code, Codex, Ollama or an API — coding agents show everything they do"),
            K("F2  agents", "run a team: a lead AI plans the work and hands it to worker AIs"),
            K("F3  your AIs", "install and sign in to AI tools, see your limits and usage"),
            K("F4–F9", "music · system · files · notes · storage · terminal (and calendar, no key)"),
            K("F10  help", "this screen"),
            Gap,
            H("five things worth knowing"),
            K("alt p", "the palette: search every app, action and theme"),
            K("alt n", "a terminal beside whatever you're looking at"),
            K("drag", "select text anywhere — it's copied when you let go"),
            K("alt v", "paste a screenshot into Claude Code, Codex or the chat"),
            K("× / alt w", "close a pane or one of your tabs"),
            Gap,
            P("Replay the tour any time: palette → \"take the tour\", or run `oriel --tour`."),
        ]},
        Topic { icon: "window", title: "keys & tabs", app: "", items: vec![
            H("apps and tabs"),
            K("F1–F10", "switch apps (F12 plays/pauses music from anywhere)"),
            K("alt t", "a new tab of your own (it starts on a launcher)"),
            K("alt 1–9", "go to one of your tabs"),
            K("double-click a tab", "rename it (also right-click → rename, or ctrl+space then ,)"),
            K("× on a tab / middle-click", "close that tab"),
            K("alt s", "hide or show the sidebar"),
            Gap,
            H("panes (splits)"),
            K("alt n", "open a terminal beside the current pane (alt enter works too outside Windows Terminal)"),
            K("alt ← ↑ ↓ →", "move between panes"),
            K("alt shift ← ↑ ↓ →", "resize the pane"),
            K("drag a divider", "resize with the mouse"),
            K("alt z", "zoom the pane (again to unzoom)"),
            K("alt w / × on the frame", "close the pane"),
            K("right-click", "menu: split, zoom, rename, new tab, close"),
            Gap,
            H("tmux-style keys: ctrl+space, then…"),
            K("|  or  v", "split right"),
            K("-", "split down"),
            K("h j k l", "move (H J K L resize)"),
            K("x  ·  z", "close pane · zoom"),
            K("c  ·  n  ·  p", "new tab · next tab · previous tab"),
            K(",  ·  &", "rename tab · close tab"),
            K("t  ·  space", "themes · palette"),
            K("ctrl+space twice", "send ctrl+space to the program"),
            Gap,
            H("copy and paste"),
            K("drag", "select text in any pane; copied when you let go (shift+drag in programs that use the mouse)"),
            K("ctrl+v / right-click", "paste text (your terminal's own paste)"),
            K("alt v", "paste a clipboard image or copied files as a path — Claude Code and Codex attach it"),
        ]},
        Topic { icon: "ai", title: "chat", app: "ai", items: vec![
            H("talking to an AI"),
            P("Type and press enter. oriel uses whichever AI you have: Claude Code, Codex, Ollama, an OpenAI-compatible server or the Anthropic API. /provider switches the AI, /model its model — both are remembered."),
            Gap,
            K("enter", "send"),
            K("shift+enter", "new line"),
            K("esc", "stop the reply (anything you queued goes back in the box)"),
            K("enter while it works", "queue a message: Claude Code reads it at its next step, others get it when the reply ends"),
            K("ctrl+x then s", "send now: stop the reply and send what you queued"),
            K("↑ on a queued message", "take it back to edit (until the AI has read it)"),
            K("ctrl+r", "regenerate the last reply"),
            K("ctrl+n  ·  ctrl+d", "new chat · delete this chat"),
            K("↑", "in an empty box: your last message"),
            K("pgup / pgdn / wheel", "scroll"),
            K("ctrl+o", "expand every tool call (diffs, command output)"),
            K("alt v", "attach a screenshot"),
            Gap,
            H("coding agents (Claude Code, Codex)"),
            P("They work in the folder oriel was started in (/cwd changes it). Every file read, edit (as a diff), command and output shows live, with their todo list pinned above the box."),
            K("/perms ask", "they ask before each change: y allow · n deny · a always allow that tool"),
            K("/perms edits", "they edit files in the folder, anything else is refused (default)"),
            K("/perms plan", "read-only"),
            K("/perms bypass", "anything, never asks — only in folders you trust"),
            P("The choice is saved for every chat. If a reply says actions were blocked, pick a looser mode."),
        ]},
        Topic { icon: "ai", title: "chat commands", app: "", items: chat_cmds },
        Topic { icon: "robot", title: "agents & lead mode", app: "agents", items: vec![
            H("a team of AIs (lead mode)"),
            P("Give one goal. A lead AI you choose plans it into small tasks and hands them to worker AIs from your roster. Each worker codes in its own copy of the repo, so they can't clash; finished work is merged one task at a time into a separate branch, with a conflict check and your build/tests first. You review the result once and merge it."),
            Gap,
            K("L", "start a lead run: goal, lead AI + model, workers at once, budget · ctrl+s starts"),
            K("R", "the roster: which worker AIs exist, their model, cost tier, strengths, budget"),
            K("w", "watch: the lead and every worker live, side by side"),
            K("enter on a card", "that agent's full transcript"),
            K("t", "take over a worker in a real terminal tab"),
            K("s", "stop the run (the lead and all workers)"),
            K("d  ·  m", "review the combined diff · merge it into your branch"),
            K("r", "resume a run after restarting oriel"),
            P("The lead sees your plan limits and sends simple work to cheap models; the run stops starting new work when its budget is spent. oriel never pushes."),
            Gap,
            H("single tasks"),
            K("n", "new task: title, prompt, which AI — it opens in its own tab"),
            K("enter", "start a task / open its agent"),
            K("d  ·  c", "diff · send feedback"),
            K("m  ·  x", "squash-merge into your branch · discard"),
            K("P", "let an AI plan tasks for a goal"),
            K("o", "pick the repo"),
            P("Status dots: ◐ working · red ● needs you · green ● finished while you were away."),
        ]},
        Topic { icon: "gauge", title: "your AIs", app: "ais", items: vec![
            H("overview (1)"),
            P("Every AI tool you have: installed, signed in, plan limits with reset countdowns, tokens and cost for today and the week."),
            K("c", "connect your real Claude 5-hour / weekly limits (asks first)"),
            H("install (2)"),
            P("12 coding CLIs — Claude Code, Codex, Kimi, OpenCode, Aider, Copilot, Cursor, Qwen, Amp, Droid, Crush, Goose — installed with the official command for your system, in a terminal you can watch."),
            K("enter  ·  l  ·  o", "install · sign in · docs"),
            H("usage (3)"),
            K("← →  ·  ↑ ↓", "which AI · scroll the days"),
            H("token saver (4)"),
            P("Frugal / Balanced / Max presets for Claude Code and Codex. Shows exactly what changes and asks first; only its own settings are touched, with a backup."),
        ]},
        Topic { icon: "music", title: "music", app: "music", items: vec![
            P("Plays your music folder (set in setup, or [music] folders in the config): cover art, a spectrum, synced lyrics, playlists."),
            K("enter", "play the selected song"),
            K("space", "play / pause (F12 from any app)"),
            K("← →  ·  n / p", "seek · next / previous"),
            K("+ / -", "volume"),
            K("s  ·  r", "shuffle · repeat"),
            K("/", "search"),
            K("tab", "library · most played · playlists"),
        ]},
        Topic { icon: "system", title: "system", app: "system", items: vec![
            P("A task manager with seven views (1–7 or tab): summary, processes, performance, startup, services, connections, system info."),
            K("c m r w n", "sort by cpu · memory · disk read · disk write · name"),
            K("t", "process tree"),
            K("/", "filter"),
            K("k", "kill the selected process (asks first)"),
        ]},
        Topic { icon: "files", title: "files", app: "files", items: vec![
            K("enter", "open a folder / preview a file"),
            K("backspace", "up a folder"),
            K("o", "open with your system's app"),
            K("p", "copy the path"),
            K("t", "a terminal in this folder"),
            K(".", "show hidden files"),
            P("The sidebar lists home, desktop, downloads, documents and your drives."),
        ]},
        Topic { icon: "notes", title: "notes", app: "notes", items: vec![
            P("Markdown notes that save as you type (the folder is set in setup)."),
            K("ctrl+n", "new note"),
            K("ctrl+e", "switch between editing and the rendered preview"),
            K("ctrl+d", "delete the note (asks first)"),
            K("ctrl+z  ·  ctrl+y", "undo · redo"),
        ]},
        Topic { icon: "calendar", title: "calendar", app: "calendar", items: vec![
            P("A month at a glance with the chosen day's plans beside it. Plans with a time pop up as a reminder when they start, whichever app you're in. The sidebar shows the next two weeks."),
            K("← → ↑ ↓", "move a day / a week (or click a day)"),
            K("[ ]  ·  wheel", "previous / next month"),
            K("t", "back to today"),
            K("a  ·  enter", "add a plan: \"2pm dentist\", \"10:30 standup\", or just \"buy milk\""),
            K("j k", "pick one of the day's plans"),
            K("e  ·  x  ·  u", "edit it · delete it · undo the delete"),
        ]},
        Topic { icon: "storage", title: "storage", app: "storage", items: vec![
            P("Views: cleanup · big folders · installed apps · get apps · search (tab to switch)."),
            K("x", "clean the selected item (asks first; review items only open in files)"),
            K("get apps", "60+ popular apps by category — enter installs (asks first)"),
            K("u", "uninstall the selected app"),
            K("/  ·  s", "filter · search the package manager for anything"),
            K("r", "rescan"),
        ]},
        Topic { icon: "term", title: "terminal", app: "terminal", items: vec![
            P("A real shell (PowerShell on Windows, your $SHELL elsewhere). Anything runs in it — including Claude Code or Codex, which then get status dots on their tab."),
            K("alt n", "another terminal beside it"),
            K("wheel", "scroll back"),
            K("drag", "copy text"),
        ]},
        Topic { icon: "theme", title: "themes & config", app: "themes", items: vec![
            K("alt p → theme", "pick a theme with live preview"),
            P("ultra is the default. terminal uses your terminal's own colours; omarchy follows your Omarchy theme live."),
            Gap,
            H("make your own"),
            P("Open themes (bottom of the sidebar, or /theme edit). Pick a colour, then change it: every change saves and recolours oriel at once. Editing a built-in theme makes your own copy on the first change."),
            K("↑ ↓", "pick a colour"),
            K("← →", "turn its hue (shift: finer)"),
            K("[ ]  ·  - =", "darker / lighter · less / more colour"),
            K("enter", "type it: #rrggbb, an ANSI name like bright-blue, or terminal"),
            K("r  ·  n  ·  o", "reset to the original · new theme · open the file"),
            P("A theme is one small file in the themes folder next to config.toml: base = \"ultra\" plus the colours you changed. Send it to a friend and it works for them too."),
            Gap,
            H("the config file"),
            P("`oriel --config` prints where it is. Everything is optional: theme, shell, prefix key, startup app, icons, notes folder, music folders, the AI providers and keys, permissions, per-AI models, the lead and the roster."),
            K("oriel update", "update to the latest release"),
            K("oriel --tour", "replay the setup and tour"),
            K("oriel <app>", "start straight in an app, e.g. oriel music"),
        ]},
        Topic { icon: "search", title: "troubleshooting", app: "", items: vec![
            K("icons are boxes", "your terminal font has no icons — install a Nerd Font, or palette → \"toggle nerd font icons\""),
            K("alt enter goes fullscreen", "Windows Terminal takes it — use alt n for a terminal"),
            K("ctrl+v won't paste images", "Windows Terminal only pastes text — use alt v"),
            K("no AI found", "your AIs (F3) → install: Claude Code, Codex and others with one key"),
            K("agents refuse actions", "/perms in chat (ask · edits · plan · bypass)"),
            K("update didn't stick", "close oriel and run the install line again"),
        ]},
    ]
}

pub struct Help {
    topics: Vec<Topic>,
    sel: usize,
    scroll: usize,
    query: String,
    searching: bool,
    hits: Vec<(Rect, usize)>,
}

impl Help {
    pub fn new() -> Help {
        Help { topics: topics(), sel: 0, scroll: 0, query: String::new(), searching: false, hits: vec![] }
    }

    /// Topics matching the search (all when empty).
    fn shown(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        (0..self.topics.len())
            .filter(|&i| {
                q.is_empty() || {
                    let t = &self.topics[i];
                    t.title.contains(&q)
                        || t.items.iter().any(|it| match it {
                            H(s) | P(s) => s.to_lowercase().contains(&q),
                            K(a, b) => a.to_lowercase().contains(&q) || b.to_lowercase().contains(&q),
                            Gap => false,
                        })
                }
            })
            .collect()
    }

    /// Jump to the topic for the app help was asked from (once per ask).
    fn pick_context(&mut self) {
        let ctx = CONTEXT.with(|c| c.borrow_mut().take());
        if let Some(i) = ctx.and_then(|c| self.topics.iter().position(|t| t.app == c)) {
            self.sel = i;
            self.scroll = 0;
            self.query.clear();
            self.searching = false;
        }
    }
}

impl Pane for Help {
    fn title(&self) -> String {
        // the frame is drawn before render() picks up a new context, so look ahead here
        let ctx = CONTEXT.with(|c| c.borrow().clone());
        let sel = ctx.and_then(|c| self.topics.iter().position(|t| t.app == c)).unwrap_or(self.sel);
        format!("help · {}", self.topics[sel].title)
    }
    fn icon(&self) -> &'static str {
        "search"
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.pick_context();
        let t = cx.theme;
        let hints: &[(&str, &str)] = if self.searching { &[("type", "to search"), ("enter", "done"), ("esc", "clear")] } else { &[("↑↓", "topic"), ("pgup/pgdn", "scroll"), ("/", "search"), ("F1–F9", "back to an app")] };
        let area = ui::hint_line(f, area, hints, t);
        let body = Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(1) };
        let topic = &self.topics[self.sel];
        let w = body.width as usize;
        let key_w = topic.items.iter().filter_map(|it| if let K(a, _) = it { Some(unicode_width::UnicodeWidthStr::width(*a)) } else { None }).max().unwrap_or(10).clamp(8, 28);
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(format!("{}{}", ui::lead(topic.icon), topic.title), Style::default().fg(t.accent).add_modifier(Modifier::BOLD))), Line::raw("")];
        let wrap = |s: &str, indent: usize, width: usize| -> Vec<String> {
            let mut out = vec![];
            let mut cur = String::new();
            for word in s.split(' ') {
                if !cur.is_empty() && unicode_width::UnicodeWidthStr::width(cur.as_str()) + 1 + unicode_width::UnicodeWidthStr::width(word) > width.saturating_sub(indent) {
                    out.push(std::mem::take(&mut cur));
                }
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(word);
            }
            if !cur.is_empty() {
                out.push(cur);
            }
            out
        };
        for it in &topic.items {
            match it {
                H(s) => {
                    if lines.len() > 2 && lines.last().is_some_and(|l| l.width() > 0) {
                        lines.push(Line::raw(""));
                    }
                    lines.push(Line::from(Span::styled(s.to_string(), Style::default().fg(t.shine).add_modifier(Modifier::BOLD))));
                }
                P(s) => {
                    for l in wrap(s, 0, w) {
                        lines.push(Line::from(Span::styled(l, Style::default().fg(t.muted))));
                    }
                }
                K(a, b) => {
                    let desc = wrap(b, key_w + 4, w);
                    for (n, d) in desc.iter().enumerate() {
                        let k = if n == 0 { a.to_string() } else { String::new() };
                        let pad = key_w.saturating_sub(unicode_width::UnicodeWidthStr::width(k.as_str()));
                        lines.push(Line::from(vec![
                            Span::styled(format!("  {k}{}", " ".repeat(pad)), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
                            Span::raw(format!("  {d}")),
                        ]));
                    }
                }
                Gap => lines.push(Line::raw("")),
            }
        }
        let h = body.height as usize;
        self.scroll = self.scroll.min(lines.len().saturating_sub(h));
        let visible: Vec<Line> = lines.into_iter().skip(self.scroll).take(h).collect();
        f.render_widget(Paragraph::new(visible), body);
    }

    fn key(&mut self, k: KeyEvent, _cx: &mut Cx) -> bool {
        if self.searching {
            match k.code {
                KeyCode::Esc => {
                    self.query.clear();
                    self.searching = false;
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => self.query.push(c),
                _ => return false,
            }
            if let Some(&first) = self.shown().first() {
                if !self.shown().contains(&self.sel) {
                    self.sel = first;
                    self.scroll = 0;
                }
            }
            return true;
        }
        let shown = self.shown();
        let pos = shown.iter().position(|&i| i == self.sel).unwrap_or(0);
        match k.code {
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.sel = shown[(pos + 1).min(shown.len().saturating_sub(1))];
                self.scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.sel = shown[pos.saturating_sub(1)];
                self.scroll = 0;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll += 10,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
            }
            KeyCode::Esc if !self.query.is_empty() => self.query.clear(),
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        match ev.kind {
            MouseEventKind::ScrollDown => self.scroll += 3,
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            _ => {}
        }
    }

    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.pick_context(); // the sidebar is drawn before the pane
        let t = cx.theme;
        self.hits.clear();
        let mut y = area.y;
        if self.searching || !self.query.is_empty() {
            let q = format!("{}{}{}", ui::lead("search"), self.query, if self.searching { "▏" } else { "" });
            f.render_widget(Paragraph::new(Span::styled(q, ui::accent(t))), Rect { y, height: 1, ..area });
            y += 2;
        }
        for i in self.shown() {
            if y >= area.bottom() {
                break;
            }
            let tp = &self.topics[i];
            let r = Rect { y, height: 1, ..area };
            ui::side_row(f, r, tp.icon, tp.title, "", i == self.sel, t);
            self.hits.push((r, i));
            y += 1;
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            if let Some(&(_, i)) = self.hits.iter().find(|(r, _)| r.contains(pos)) {
                self.sel = i;
                self.scroll = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;

    #[test]
    fn help_topics_and_context() {
        let mut k = Kit::new();
        set_context("agents");
        let mut h = Help::new();
        let s = k.render_html(&mut h, 150, 44, "target/snap/help-agents.html");
        assert!(s.contains("lead mode") && s.contains("start a lead run"), "{s}");
        // every chat command is listed on the commands topic
        h.sel = h.topics.iter().position(|t| t.title == "chat commands").unwrap();
        let s = k.render(&mut h, 150, 60);
        for (c, _, _) in crate::panes::chat::COMMANDS {
            assert!(s.contains(c), "{c} missing from help");
        }
        // search narrows the topics
        k.key(&mut h, KeyCode::Char('/'));
        for c in "clipboard".chars() {
            k.key(&mut h, KeyCode::Char(c));
        }
        let side = k.render_side(&mut h, 30, 20);
        assert!(side.contains("keys & tabs") && !side.contains("music"), "{side}");
    }
}
