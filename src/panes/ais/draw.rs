//! Drawing, keys and mouse for the "your AIs" pane (state and background work live in ais.rs).

use super::catalog::{CLIS, Cli};
use super::limits::{Limits, Window, is_reset};
use super::usage::{Src, SrcSum, Tot};
use super::util::{ago, day_label, dur, hhmm, money, now, spark, tok};
use super::{Ais, Ask, VIEWS, View, saver};
use crate::pane::{Cx, Pane};
use crate::theme::Theme;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};

const AMBER: Color = Color::Rgb(0xe0, 0xa0, 0x40);

// ------------------------------------------------------------------ small drawing helpers

fn put(f: &mut Frame, r: Rect, y: u16, spans: Vec<Span<'static>>) {
    if y < r.y || y >= r.bottom() {
        return;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), Rect { y, height: 1, ..r });
}

fn s(text: impl Into<String>, st: Style) -> Span<'static> {
    Span::styled(text.into(), st)
}

fn pad(text: &str, w: usize) -> String {
    let t = ui::fit(text, w);
    let tw = unicode_width::UnicodeWidthStr::width(t.as_str());
    format!("{t}{}", " ".repeat(w.saturating_sub(tw)))
}

fn rpad(text: &str, w: usize) -> String {
    let t = ui::fit(text, w);
    let tw = unicode_width::UnicodeWidthStr::width(t.as_str());
    format!("{}{t}", " ".repeat(w.saturating_sub(tw)))
}

/// A rounded card: title in the top border (accent when `lit`), optional right-hand title, returns the inner
/// rect with one column of padding each side.
fn card(f: &mut Frame, r: Rect, title: &str, right: Option<&str>, lit: bool, sel: bool, t: &Theme) -> Rect {
    let border = if sel { t.accent } else { t.frame };
    let tstyle = if lit { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { Style::default().fg(t.muted).add_modifier(Modifier::BOLD) };
    let mut b = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(border)).title(Line::from(s(format!(" {title} "), tstyle)));
    if let Some(rt) = right {
        b = b.title(Line::from(s(format!(" {rt} "), ui::muted(t))).right_aligned());
    }
    let inner = b.inner(r);
    f.render_widget(b, r);
    Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner }
}

fn pct_color(p: f64, t: &Theme) -> Color {
    if p >= 90.0 {
        t.danger
    } else if p >= 70.0 {
        AMBER
    } else {
        t.accent
    }
}

fn bar(p: f64, w: usize, t: &Theme) -> Vec<Span<'static>> {
    let n = ((p.clamp(0.0, 100.0) / 100.0) * w as f64).round() as usize;
    vec![s("█".repeat(n.min(w)), Style::default().fg(pct_color(p, t))), s("░".repeat(w - n.min(w)), Style::default().fg(t.frame))]
}

/// "5-hour   ████████░░░░░░░░  34%   resets in 2h 14m"
fn limit_line(w: &Window, width: usize, now: i64, t: &Theme) -> Vec<Span<'static>> {
    let reset = is_reset(w, now);
    let pct = if reset { 0.0 } else { w.pct };
    let tail = match w.resets_at {
        _ if reset => "reset since (0% until next use)".to_string(),
        Some(r) => format!("resets in {}", dur(r - now)),
        None => String::new(),
    };
    let bw = width.saturating_sub(9 + 6 + 2 + tail.len()).clamp(6, 30);
    let mut v = vec![s(pad(&w.label, 9), ui::muted(t))];
    v.extend(bar(pct, bw, t));
    v.push(s(format!(" {:>4.0}%  ", pct), Style::default().fg(pct_color(pct, t)).add_modifier(Modifier::BOLD)));
    v.push(s(tail, ui::muted(t)));
    v
}

fn tot_line(label: &str, x: &Tot, priced: bool, t: &Theme) -> Vec<Span<'static>> {
    let mut v = vec![s(pad(label, 9), ui::muted(t)), s(pad(&format!("{} tok", tok(x.tokens())), 11), Style::default().add_modifier(Modifier::BOLD))];
    if priced {
        v.push(s(pad(&money(x.cost), 9), ui::bold_accent(t)));
    } else {
        v.push(s(pad("no price", 9), ui::muted(t)));
    }
    if let Some(h) = x.cache_hit() {
        v.push(s(format!("cache {h:.0}%  "), ui::muted(t)));
    }
    v.push(s(format!("{} req", x.n), ui::muted(t)));
    v
}

impl Ais {
    fn src_of(id: &str) -> Option<Src> {
        super::usage::SRCS.iter().copied().find(|s| s.id() == id)
    }

    fn cli_icon(id: &str) -> &'static str {
        match id {
            "claude" => "claude",
            "codex" => "robot",
            _ => "ai",
        }
    }

    // ------------------------------------------------------------------ overview

    /// Which CLIs get a card: the big three always, anything installed, anything with usage logs.
    fn card_list(&self) -> Vec<usize> {
        CLIS.iter()
            .enumerate()
            .filter(|(i, c)| {
                matches!(c.id, "claude" | "codex" | "kimi")
                    || self.found[*i].as_ref().is_some_and(|f| f.installed())
                    || Self::src_of(c.id).and_then(|s| self.sum(s)).is_some_and(|s| s.all.n > 0)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn status_spans(&self, i: usize, t: &Theme) -> Vec<Span<'static>> {
        let c = &CLIS[i];
        match &self.found[i] {
            None if self.live => vec![s("checking…", ui::muted(t))],
            None => vec![s("not checked", ui::muted(t))],
            Some(f) if f.installed() => {
                let mut v = vec![s("✓ installed", Style::default().fg(t.good).add_modifier(Modifier::BOLD))];
                if let Some(ver) = &f.version {
                    v.push(s(format!(" {ver}"), Style::default()));
                }
                v.push(s("  ·  ", ui::muted(t)));
                match &f.signed {
                    Some(h) if h.is_empty() => v.push(s(c.note.to_string(), ui::muted(t))),
                    Some(h) => v.push(s(if h == "yes" { "signed in".to_string() } else { format!("signed in ({h})") }, Style::default().fg(t.good))),
                    None => v.push(s("not signed in — l on the install view", Style::default().fg(AMBER))),
                }
                v
            }
            Some(_) => vec![s("not installed", ui::muted(t)), s("  ·  press 2, then enter to install", Style::default().fg(t.frame))],
        }
    }

    fn draw_cli_card(&self, f: &mut Frame, r: Rect, i: usize, t: &Theme) {
        let c: &Cli = &CLIS[i];
        let installed = self.found[i].as_ref().is_some_and(|f| f.installed());
        let src = Self::src_of(c.id);
        let sum = src.and_then(|s| self.sum(s)).filter(|s| s.all.n > 0);
        let lim: Option<&Limits> = match c.id {
            "claude" => self.claude_lim.as_ref(),
            "codex" => self.codex_lim.as_ref(),
            _ => None,
        };
        let right = match (c.id, lim) {
            ("claude", Some(l)) if l.updated > 0 => Some(format!("limits {}", ago(now() - l.updated))),
            ("codex", Some(l)) => l.plan.as_ref().map(|p| format!("{p} plan")),
            _ => sum.filter(|s| s.last > 0).map(|s| format!("last used {}", ago(now() - s.last))),
        };
        let inner = card(f, r, &format!("{}{}", ui::lead(Self::cli_icon(c.id)), c.name), right.as_deref(), installed || sum.is_some(), false, t);
        let w = inner.width as usize;
        let mut y = inner.y;
        put(f, inner, y, self.status_spans(i, t));
        y += 1;
        let n = now();
        match lim.filter(|l| !l.windows.is_empty()) {
            Some(l) => {
                for win in l.windows.iter().take(2) {
                    put(f, inner, y, limit_line(win, w, n, t));
                    y += 1;
                }
                if l.windows.len() == 1 {
                    y += 1;
                }
            }
            None => {
                let msg = match c.id {
                    "claude" => vec![s(pad("limits", 9), ui::muted(t)), s("not connected — ", ui::muted(t)), s("c", ui::bold_accent(t)), s(" connects your real 5-hour / weekly %", ui::muted(t))],
                    "codex" => vec![s(pad("limits", 9), ui::muted(t)), s("none recorded yet (they arrive with Codex's next reply)", ui::muted(t))],
                    _ => vec![s(pad("limits", 9), ui::muted(t)), s("not published by this CLI", Style::default().fg(t.frame))],
                };
                put(f, inner, y, msg);
                y += 2;
            }
        }
        match sum {
            Some(su) => {
                let priced = su.src.priced();
                put(f, inner, y, tot_line("today", &su.today, priced, t));
                put(f, inner, y + 1, tot_line("7 days", &su.week, priced, t));
                let sp = spark(&su.spark);
                put(f, inner, y + 2, vec![s(pad("14 days", 9), ui::muted(t)), s(sp, ui::accent(t)), s(format!("  {} all time{}", tok(su.all.tokens()), if priced { format!(" · {}", money(su.all.cost)) } else { String::new() }), ui::muted(t))]);
            }
            None => {
                let msg = if self.usage.is_none() { "reading usage logs…" } else if src.is_some() { "no local usage logs yet" } else { "keeps no local usage logs oriel can read" };
                put(f, inner, y, vec![s(pad("usage", 9), ui::muted(t)), s(msg, ui::muted(t))]);
            }
        }
    }

    fn draw_ollama_card(&self, f: &mut Frame, r: Rect, t: &Theme) {
        let o = self.ollama.as_ref();
        let lit = o.is_some_and(|o| o.installed);
        let right = o.and_then(|o| o.version.clone()).map(|v| format!("v{v}"));
        let inner = card(f, r, &format!("{}Ollama", ui::lead("system")), right.as_deref(), lit, false, t);
        let mut y = inner.y;
        match o {
            None => put(f, inner, y, vec![s(if self.live { "checking…" } else { "not checked" }, ui::muted(t))]),
            Some(o) if !o.installed => put(f, inner, y, vec![s("not installed", ui::muted(t)), s("  ·  local models, no limits — ollama.com", Style::default().fg(t.frame))]),
            Some(o) => {
                let st = if o.running { s("✓ running", Style::default().fg(t.good).add_modifier(Modifier::BOLD)) } else { s("installed · not running (ollama serve)", Style::default().fg(AMBER)) };
                put(f, inner, y, vec![st, s("  ·  local, no plan limits", ui::muted(t))]);
                y += 1;
                let disk: u64 = o.models.iter().map(|m| m.size).sum();
                put(f, inner, y, vec![s(pad("models", 9), ui::muted(t)), s(format!("{} installed", o.models.len()), Style::default().add_modifier(Modifier::BOLD)), s(format!(" · {} on disk", ui::human_bytes(disk)), ui::muted(t))]);
                y += 1;
                let loaded = if o.loaded.is_empty() { "nothing loaded right now".to_string() } else { o.loaded.iter().map(|(n, v)| format!("{n} ({})", ui::human_bytes(*v))).collect::<Vec<_>>().join(", ") };
                put(f, inner, y, vec![s(pad("loaded", 9), ui::muted(t)), s(ui::fit(&loaded, inner.width as usize - 9), if o.loaded.is_empty() { ui::muted(t) } else { ui::accent(t) })]);
                y += 1;
                for m in o.models.iter().take((inner.bottom().saturating_sub(y)) as usize) {
                    put(f, inner, y, vec![s(" ".repeat(9), Style::default()), s(pad(&m.name, 28), Style::default()), s(format!("{} {}  {}", m.params, m.quant, ui::human_bytes(m.size)), ui::muted(t))]);
                    y += 1;
                }
            }
        }
    }

    fn draw_overview(&mut self, f: &mut Frame, body: Rect, t: &Theme) {
        let today = self.today_all();
        let (inst, checked) = self.installed_count();
        let mut head = vec![s(" today ", ui::muted(t)), s(format!("{} tokens", tok(today.tokens())), Style::default().add_modifier(Modifier::BOLD))];
        head.push(s(" · ", ui::muted(t)));
        head.push(s(money(today.cost), ui::bold_accent(t)));
        head.push(s(" API-equivalent", ui::muted(t)));
        if checked > 0 {
            head.push(s(format!("   ·   {inst} of {} coding CLIs installed", CLIS.len()), ui::muted(t)));
        }
        match (&self.stats, self.scanning && self.usage.is_none()) {
            (_, true) => head.push(s("   ·   reading usage logs…", ui::muted(t))),
            (Some(st), _) => head.push(s(format!("   ·   {} log files", st.files), Style::default().fg(t.frame))),
            _ => {}
        }
        put(f, body, body.y, head);
        let mut cards: Vec<Option<usize>> = self.card_list().into_iter().map(Some).collect();
        cards.insert(cards.len().min(3), None); // Ollama after the big three
        let top = body.y + 2;
        let cols: u16 = if body.width >= 110 { 2 } else { 1 };
        let ch: u16 = 8;
        let cw = (body.width.saturating_sub(cols - 1)) / cols;
        let mut last_y = top;
        for (k, c) in cards.iter().enumerate() {
            let (row, col) = (k as u16 / cols, k as u16 % cols);
            let y = top + row * (ch + 1);
            if y + ch > body.bottom().saturating_sub(2) {
                break;
            }
            let r = Rect { x: body.x + col * (cw + 1), y, width: if col == cols - 1 { body.width - col * (cw + 1) } else { cw }, height: ch };
            match c {
                Some(i) => self.draw_cli_card(f, r, *i, t),
                None => self.draw_ollama_card(f, r, t),
            }
            last_y = y + ch;
        }
        // everything else, on one line
        let shown = self.card_list();
        let missing: Vec<&str> = CLIS.iter().enumerate().filter(|(i, _)| !shown.contains(i) && self.found[*i].as_ref().is_some_and(|f| !f.installed())).map(|(_, c)| c.name).collect();
        if !missing.is_empty() {
            let y = (last_y + 1).min(body.bottom().saturating_sub(1));
            let text = ui::fit(&missing.join(" · "), body.width.saturating_sub(34) as usize);
            put(f, body, y, vec![s(" also available: ", ui::muted(t)), s(text, Style::default()), s("   → 2 installs", ui::accent(t))]);
        }
    }

    // ------------------------------------------------------------------ install

    fn draw_install(&mut self, f: &mut Frame, body: Rect, focused: bool, t: &Theme) {
        self.row_hits.clear();
        let detail_h = 7u16;
        let tr = Rect { height: body.height.saturating_sub(detail_h + 1), ..body };
        let inner = card(f, tr, &format!("{}coding CLIs for {}", ui::lead("package"), self.os.label()), Some("✓ = found on this machine"), true, false, t);
        let w = inner.width as usize;
        let (cv, cs) = (11usize, 22usize);
        let cn = 16usize;
        let cd = 30usize.min(w / 4);
        let ci = w.saturating_sub(2 + cn + cv + cs + cd + 5);
        let head = vec![s(format!("  {}{}{}{}{}", pad("cli", cn + 1), pad("version", cv + 1), pad("signed in", cs + 1), pad("install with", ci + 1), pad("what it is", cd)), Style::default().add_modifier(Modifier::BOLD))];
        put(f, inner, inner.y, head);
        let sel = self.sel[View::Install.i()];
        let rows = inner.height.saturating_sub(1) as usize;
        let off = sel.saturating_sub(rows.saturating_sub(1));
        for (k, (i, c)) in CLIS.iter().enumerate().skip(off).take(rows).enumerate() {
            let y = inner.y + 1 + k as u16;
            let on = i == sel;
            let hi = if on && focused { Style::default().bg(t.frame) } else { Style::default() };
            let fnd = self.found[i].as_ref();
            let inst = fnd.is_some_and(|f| f.installed());
            let mark = match fnd {
                None => s("… ", hi.fg(t.muted)),
                Some(_) if inst => s("✓ ", hi.fg(t.good).add_modifier(Modifier::BOLD)),
                Some(_) => s("· ", hi.fg(t.frame)),
            };
            let ver = fnd.and_then(|f| f.version.clone()).unwrap_or_default();
            let signed = match fnd {
                Some(f) if f.installed() => match &f.signed {
                    Some(h) if h.is_empty() => ("—".to_string(), hi.fg(t.muted)),
                    Some(h) => (if h == "yes" { "yes".into() } else { h.clone() }, hi.fg(t.good)),
                    None => ("no".into(), hi.fg(AMBER)),
                },
                _ => (String::new(), hi),
            };
            let (how, how_st) = match &self.picks[i] {
                Some((cmd, true)) => (cmd.clone(), hi.fg(if inst { t.muted } else { t.accent })),
                Some((cmd, false)) => (format!("{cmd}  (needs {})", cmd.split_whitespace().find(|w| *w != "sudo").unwrap_or("?")), hi.fg(AMBER)),
                None => (if c.note.is_empty() { "not packaged here".into() } else { c.note.to_string() }, hi.fg(t.muted)),
            };
            let name_st = if on { hi.fg(t.accent).add_modifier(Modifier::BOLD) } else if inst { hi.add_modifier(Modifier::BOLD) } else { hi };
            let spans = vec![
                mark,
                s(pad(c.name, cn + 1), name_st),
                s(pad(&ver, cv + 1), hi.fg(t.muted)),
                s(pad(&signed.0, cs + 1), signed.1),
                s(pad(&how, ci + 1), how_st),
                s(pad(c.desc, cd), hi.fg(t.muted)),
            ];
            let row = Rect { y, height: 1, ..inner };
            put(f, inner, y, spans);
            self.row_hits.push((row, i));
        }
        // details for the selected CLI
        let c = &CLIS[sel];
        let dr = Rect { y: tr.bottom() + 1, height: detail_h, ..body };
        let di = card(f, dr, &format!("{}{}", ui::lead(Self::cli_icon(c.id)), c.name), Some(c.bin), true, false, t);
        let dw = di.width as usize;
        let lab = |x: &str| s(pad(x, 10), ui::muted(t));
        let fnd = self.found[sel].as_ref();
        let status = match fnd {
            Some(f) if f.installed() => format!("installed at {}", f.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
            Some(_) => "not installed".into(),
            None => "checking…".into(),
        };
        put(f, di, di.y, vec![lab("status"), s(ui::fit(&status, dw - 10), Style::default())]);
        let inst_line = match &self.picks[sel] {
            Some((cmd, ready)) => vec![lab("install"), s(ui::fit(cmd, dw.saturating_sub(40)), ui::accent(t)), s(if *ready { "   enter runs it in a terminal pane (asks first)".to_string() } else { format!("   needs {} first", cmd.split_whitespace().find(|w| *w != "sudo").unwrap_or("?")) }, if *ready { ui::muted(t) } else { Style::default().fg(AMBER) })],
            None => vec![lab("install"), s(format!("nothing for {} — {}", self.os.label(), if c.note.is_empty() { "see the docs" } else { c.note }), ui::muted(t))],
        };
        put(f, di, di.y + 1, inst_line);
        let login = match c.login {
            Some(l) => vec![lab("sign in"), s(l.to_string(), ui::accent(t)), s("   l runs it (browser or device code)", ui::muted(t))],
            None => vec![lab("sign in"), s(if c.note.is_empty() { "no sign-in" } else { c.note }.to_string(), ui::muted(t))],
        };
        put(f, di, di.y + 2, login);
        put(f, di, di.y + 3, vec![lab("docs"), s(c.docs.to_string(), Style::default()), s("   o opens it", ui::muted(t))]);
        put(f, di, di.y + 4, vec![lab("verify"), s(format!("{} {}", c.bin, c.ver.join(" ")), ui::muted(t))]);
    }

    // ------------------------------------------------------------------ usage

    fn draw_usage(&mut self, f: &mut Frame, body: Rect, t: &Theme) {
        let srcs = self.usage_srcs();
        if srcs.is_empty() {
            let msg = if self.usage.is_none() { "reading usage logs… (the first read of a big history takes a few seconds; after that only new lines are read)" } else { "no usage logs found — Claude Code, Codex, Kimi, Gemini and OpenCode keep them locally once you use them" };
            put(f, body, body.y + 1, vec![s(format!(" {msg}"), ui::muted(t))]);
            return;
        }
        self.usrc = self.usrc.min(srcs.len() - 1);
        let src = srcs[self.usrc];
        // source switcher
        let mut tabs = vec![s(" ", Style::default())];
        for (k, sr) in srcs.iter().enumerate() {
            let on = k == self.usrc;
            tabs.push(s(format!(" {} ", sr.label()), if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { ui::muted(t) }));
            tabs.push(s("  ", Style::default()));
        }
        tabs.push(s("←/→ switch", Style::default().fg(t.frame)));
        put(f, body, body.y, tabs);
        let Some(su) = self.sum(src).cloned() else { return };
        let area = Rect { y: body.y + 2, height: body.height.saturating_sub(2), ..body };
        let rw = if area.width >= 120 { 58 } else { 0 };
        let left = Rect { width: area.width.saturating_sub(if rw > 0 { rw + 1 } else { 0 }), ..area };
        self.draw_daily(f, left, &su, t);
        if rw > 0 {
            let right = Rect { x: left.right() + 1, width: rw, ..area };
            self.draw_usage_side(f, right, &su, t);
        }
    }

    fn draw_daily(&mut self, f: &mut Frame, r: Rect, su: &SrcSum, t: &Theme) {
        let priced = su.src.priced();
        let sub = format!("{} log files · {}", su.files, if priced { "$ = API-equivalent at list prices" } else { "no price table for this one" });
        let inner = card(f, r, &format!("{}daily · {}", ui::lead("chart"), su.src.label()), Some(&sub), true, false, t);
        let w = inner.width as usize;
        let nw = 9usize;
        let dw = 11usize;
        let bw = w.saturating_sub(dw + nw * 6 + 7).min(24);
        let head = format!("{}{}{}{}{}{}{} {}", pad("day", dw), rpad("input", nw), rpad("output", nw), rpad("cache wr", nw), rpad("cache rd", nw), rpad("total", nw), rpad("$", nw), "");
        put(f, inner, inner.y, vec![s(head, Style::default().add_modifier(Modifier::BOLD))]);
        let rows = inner.height.saturating_sub(3) as usize;
        let days: Vec<&(i64, Tot)> = su.days.iter().rev().collect();
        let v = View::Usage.i();
        self.scroll[v] = self.scroll[v].min(days.len().saturating_sub(rows));
        let max = days.iter().map(|d| d.1.tokens()).max().unwrap_or(1).max(1);
        let today = super::util::day_of(now(), self.off);
        for (k, (day, x)) in days.iter().skip(self.scroll[v]).take(rows).enumerate() {
            let y = inner.y + 1 + k as u16;
            let label = if *day == today { "today".to_string() } else { day_label(*day) };
            let n = ((x.tokens() as f64 / max as f64) * bw as f64).round() as usize;
            let cost = if priced { money(x.cost) } else { "—".into() };
            put(f, inner, y, vec![
                s(pad(&label, dw), if *day == today { ui::bold_accent(t) } else { Style::default() }),
                s(rpad(&tok(x.inp), nw), ui::muted(t)),
                s(rpad(&tok(x.out), nw), ui::muted(t)),
                s(rpad(&tok(x.cw), nw), ui::muted(t)),
                s(rpad(&tok(x.cr), nw), ui::muted(t)),
                s(rpad(&tok(x.tokens()), nw), Style::default().add_modifier(Modifier::BOLD)),
                s(rpad(&cost, nw), ui::accent(t)),
                s("  ", Style::default()),
                s("▆".repeat(n.min(bw)), Style::default().fg(crate::theme::mix(t.frame, t.accent, 0.55))),
            ]);
        }
        // totals
        let y = inner.bottom().saturating_sub(1);
        ui::rule(f, Rect { y: y - 1, height: 1, ..inner }, t);
        let a = &su.all;
        let hit = a.cache_hit().map(|h| format!("   cache hit {h:.0}%")).unwrap_or_default();
        put(f, inner, y, vec![
            s(pad(&format!("{} days", su.days.len()), dw), Style::default().add_modifier(Modifier::BOLD)),
            s(rpad(&tok(a.inp), nw), Style::default()),
            s(rpad(&tok(a.out), nw), Style::default()),
            s(rpad(&tok(a.cw), nw), Style::default()),
            s(rpad(&tok(a.cr), nw), Style::default()),
            s(rpad(&tok(a.tokens()), nw), Style::default().add_modifier(Modifier::BOLD)),
            s(rpad(&if priced { money(a.cost) } else { "—".into() }, nw), ui::bold_accent(t)),
            s(format!("{hit}   {} requests", a.n), ui::muted(t)),
        ]);
    }

    fn draw_usage_side(&self, f: &mut Frame, r: Rect, su: &SrcSum, t: &Theme) {
        let priced = su.src.priced();
        let val = |x: &Tot| if priced { money(x.cost) } else { tok(x.tokens()) };
        let blocks_h = if su.src == Src::Claude { 9u16 } else { (su.models.len() as u16 + 2).clamp(3, 9) };
        let rest = r.height.saturating_sub(blocks_h + if blocks_h > 0 { 1 } else { 0 });
        let ph = (rest / 2).max(4);
        // projects
        let pr = Rect { height: ph, ..r };
        let pi = card(f, pr, "top projects", Some(if priced { "by cost" } else { "by tokens" }), true, false, t);
        let w = pi.width as usize;
        let maxv = su.projects.first().map(|p| if priced { p.1.cost } else { p.1.tokens() as f64 }).unwrap_or(1.0).max(1e-9);
        for (k, (name, x)) in su.projects.iter().take(pi.height as usize).enumerate() {
            let v = if priced { x.cost } else { x.tokens() as f64 };
            let n = ((v / maxv) * 12.0).round() as usize;
            put(f, pi, pi.y + k as u16, vec![
                s(pad(name, w.saturating_sub(24)), Style::default()),
                s(format!("{:<12}", "▆".repeat(n.min(12))), Style::default().fg(crate::theme::mix(t.frame, t.accent, 0.55))),
                s(rpad(&val(x), 10), ui::accent(t)),
            ]);
        }
        // sessions
        let sr = Rect { y: pr.bottom(), height: rest - ph, ..r };
        let si = card(f, sr, "top sessions", Some(if priced { "by cost" } else { "by tokens" }), true, false, t);
        let w = si.width as usize;
        for (k, x) in su.sessions.iter().take(si.height as usize).enumerate() {
            let when = if x.last > 0 { format!("{} {}", day_label(super::util::day_of(x.last, self.off)).get(4..).unwrap_or(""), hhmm(x.last, self.off)) } else { String::new() };
            put(f, si, si.y + k as u16, vec![
                s(pad(&x.project, w.saturating_sub(36)), Style::default()),
                s(pad(&x.id.chars().take(8).collect::<String>(), 10), Style::default().fg(t.frame)),
                s(pad(&when, 14), ui::muted(t)),
                s(rpad(&val(&x.tot), 10), ui::accent(t)),
            ]);
        }
        if su.src != Src::Claude {
            let br = Rect { y: sr.bottom(), height: blocks_h, ..r };
            let bi = card(f, br, "models", None, true, false, t);
            for (k, (m, x)) in su.models.iter().take(bi.height as usize).enumerate() {
                put(f, bi, bi.y + k as u16, vec![s(pad(m, bi.width as usize - 30), Style::default()), s(rpad(&format!("{} req", x.n), 10), ui::muted(t)), s(rpad(&tok(x.tokens()), 9), ui::muted(t)), s(rpad(&val(x), 10), ui::accent(t))]);
            }
        } else {
            let br = Rect { y: sr.bottom(), height: blocks_h, ..r };
            let bi = card(f, br, "5-hour blocks", Some("estimate: this machine only"), true, false, t);
            let n = now();
            for (k, b) in su.blocks.iter().take(bi.height as usize).enumerate() {
                let day = day_label(super::util::day_of(b.start, self.off));
                let span = format!("{} {}–{}", day.get(4..).unwrap_or(""), hhmm(b.start, self.off), hhmm(b.end, self.off));
                let tail = if b.active { format!("active · ends in {}", dur(b.end - n)) } else { String::new() };
                put(f, bi, bi.y + k as u16, vec![
                    s(pad(&span, 20), if b.active { ui::bold_accent(t) } else { Style::default() }),
                    s(rpad(&tok(b.tot.tokens()), 8), ui::muted(t)),
                    s(rpad(&money(b.tot.cost), 9), ui::accent(t)),
                    s(format!("  {tail}"), Style::default().fg(t.good)),
                ]);
            }
        }
    }

    // ------------------------------------------------------------------ token saver

    fn draw_saver(&mut self, f: &mut Frame, body: Rect, focused: bool, t: &Theme) {
        self.row_hits.clear();
        let rw = if body.width >= 120 { 56 } else { 0 };
        let left = Rect { width: body.width.saturating_sub(if rw > 0 { rw + 1 } else { 0 }), ..body };
        let matches = self.readouts.as_ref().and_then(|r| r.matches);
        let ph = ((left.height + 1) / saver::PRESETS.len() as u16).saturating_sub(1).min(11);
        for (k, p) in saver::PRESETS.iter().enumerate() {
            let r = Rect { y: left.y + k as u16 * (ph + 1), height: ph, ..left };
            if r.bottom() > left.bottom() {
                break;
            }
            let on = k == self.preset;
            let right = if matches == Some(p.name) { Some("✓ what Claude uses now") } else { None };
            let inner = card(f, r, p.name, right, on, on && focused, t);
            self.row_hits.push((r, k));
            let w = inner.width as usize;
            let mut lines: Vec<Vec<Span<'static>>> = vec![vec![s(p.blurb.to_string(), if on { Style::default() } else { ui::muted(t) })], vec![]];
            let claude: Vec<String> = p.claude.iter().map(|(k, v)| format!("{k} {}", match v { saver::V::S(x) => x.to_string(), saver::V::B(b) => b.to_string() })).collect();
            lines.push(vec![s(pad("Claude", 8), ui::bold_accent(t)), s(claude.join(" · "), Style::default())]);
            let (set, clear): (Vec<_>, Vec<_>) = p.env.iter().partition(|(_, v)| v.is_some());
            if !set.is_empty() {
                let txt = set.iter().map(|(k, v)| format!("{k}={}", v.unwrap_or(""))).collect::<Vec<_>>().join("  ");
                for chunk in wrap(&format!("env {txt}"), w.saturating_sub(8)) {
                    lines.push(vec![s(" ".repeat(8), Style::default()), s(chunk, ui::muted(t))]);
                }
            }
            if !clear.is_empty() {
                let txt = clear.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(" ");
                for chunk in wrap(&format!("removes {txt}"), w.saturating_sub(8)) {
                    lines.push(vec![s(" ".repeat(8), Style::default()), s(chunk, Style::default().fg(t.frame))]);
                }
            }
            let codex = p.codex.iter().map(|(k, v)| format!("{k}={}", v.trim_matches('"'))).collect::<Vec<_>>().join("  ");
            let mut first = true;
            for chunk in wrap(&format!("[profiles.{}]  {codex}", p.profile), w.saturating_sub(8)) {
                lines.push(vec![s(if first { pad("Codex", 8) } else { " ".repeat(8) }, ui::bold_accent(t)), s(chunk, Style::default())]);
                first = false;
            }
            for (j, l) in lines.into_iter().enumerate() {
                put(f, inner, inner.y + j as u16, l);
            }
        }
        if rw == 0 {
            return;
        }
        let right = Rect { x: left.right() + 1, width: rw, ..body };
        let nh = right.height.saturating_sub(saver::TIPS.len() as u16 + 3);
        let ri = card(f, Rect { height: nh, ..right }, &format!("{}right now", ui::lead("gauge")), None, true, false, t);
        let mut y = ri.y;
        let w = ri.width as usize;
        let lab = |x: &str| s(pad(x, 14), ui::muted(t));
        match &self.readouts {
            None => put(f, ri, y, vec![s("reading your settings…", ui::muted(t))]),
            Some(r) => {
                if let Some(e) = &r.settings_error {
                    put(f, ri, y, vec![s(ui::fit(e, w), Style::default().fg(t.danger))]);
                    y += 1;
                }
                put(f, ri, y, vec![s("Claude settings.json", Style::default().add_modifier(Modifier::BOLD))]);
                y += 1;
                if r.current.is_empty() {
                    put(f, ri, y, vec![s("  (no settings.json yet)", ui::muted(t))]);
                    y += 1;
                }
                for (k, v) in &r.current {
                    put(f, ri, y, vec![s(format!("  {}", pad(k, w.saturating_sub(22))), ui::muted(t)), s(ui::fit(v, 20), Style::default())]);
                    y += 1;
                }
                put(f, ri, y, vec![lab("  preset"), s(r.matches.unwrap_or("custom (no preset matches)").to_string(), ui::accent(t))]);
                y += 2;
                let md = match r.claude_md {
                    Some((lines, bytes)) => {
                        let warn = lines > 200;
                        vec![lab("CLAUDE.md"), s(format!("{lines} lines"), if warn { Style::default().fg(AMBER).add_modifier(Modifier::BOLD) } else { Style::default().add_modifier(Modifier::BOLD) }), s(format!(" · ~{} tokens every request{}", tok(bytes as u64 / 4), if warn { " · aim for <200" } else { "" }), ui::muted(t))]
                    }
                    None => vec![lab("CLAUDE.md"), s("none (global)", ui::muted(t))],
                };
                put(f, ri, y, md);
                y += 1;
                let mcp = if r.mcp.is_empty() { "none".to_string() } else { format!("{}: {}", r.mcp.len(), r.mcp.join(", ")) };
                let mut first = true;
                for chunk in wrap(&mcp, w.saturating_sub(14)).into_iter().take(3) {
                    put(f, ri, y, vec![if first { lab("MCP servers") } else { s(" ".repeat(14), Style::default()) }, s(chunk, Style::default())]);
                    first = false;
                    y += 1;
                }
                if r.project_mcp > 0 {
                    put(f, ri, y, vec![s(" ".repeat(14), Style::default()), s(format!("+ {} projects with their own", r.project_mcp), ui::muted(t))]);
                    y += 1;
                }
                if let Some(h) = self.sum(Src::Claude).and_then(|s| s.week.cache_hit()) {
                    put(f, ri, y, vec![lab("cache hits"), s(format!("{h:.0}%"), Style::default().add_modifier(Modifier::BOLD)), s(" of Claude input, last 7 days", ui::muted(t))]);
                    y += 1;
                }
                let prof = if r.codex_profiles.is_empty() { "none".to_string() } else { r.codex_profiles.join(", ") };
                put(f, ri, y, vec![lab("Codex profiles"), s(ui::fit(&prof, w.saturating_sub(14)), Style::default())]);
                y += 1;
                let sl = match &r.status_line {
                    Some(c) if c.contains("usage-sink") => s("connected (oriel usage-sink)", Style::default().fg(t.good)),
                    Some(c) => s(ui::fit(&format!("yours: {c}"), w.saturating_sub(14)), ui::muted(t)),
                    None => s("not set — c on the overview connects limits", ui::muted(t)),
                };
                put(f, ri, y, vec![lab("status line"), sl]);
            }
        }
        let ti = card(f, Rect { y: right.y + nh, height: right.height - nh, ..right }, "tips", None, true, false, t);
        for (k, tip) in saver::TIPS.iter().enumerate() {
            put(f, ti, ti.y + k as u16, vec![s("• ", ui::accent(t)), s(ui::fit(tip, ti.width as usize - 2), ui::muted(t))]);
        }
    }

    // ------------------------------------------------------------------ popups

    fn draw_ask(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let (title, mut lines): (String, Vec<Line<'static>>) = match &self.ask {
            None => return,
            Some(Ask::Install { cli, cmd, ready }) => {
                let c = &CLIS[*cli];
                let mut v = vec![
                    Line::from(s(format!("install {}?", c.name), Style::default().fg(t.danger).add_modifier(Modifier::BOLD))),
                    Line::raw(""),
                    Line::from(s("runs this in a terminal pane, so you see and answer every prompt:", ui::muted(t))),
                    Line::from(s(format!("  {cmd}"), ui::accent(t))),
                ];
                if !ready {
                    v.push(Line::from(s(format!("  heads up: {} wasn't found on PATH, so this will probably fail", cmd.split_whitespace().find(|w| *w != "sudo").unwrap_or("?")), Style::default().fg(AMBER))));
                }
                if self.found[*cli].as_ref().is_some_and(|f| f.installed()) {
                    v.push(Line::from(s("  it's already installed — this reinstalls / updates it", Style::default().fg(AMBER))));
                }
                v.push(Line::raw(""));
                v.push(Line::from(s(format!("from {}", c.docs), Style::default().fg(t.frame))));
                ("are you sure?".into(), v)
            }
            Some(Ask::Edit(p)) => {
                let mut v = vec![Line::from(s(p.title.clone(), Style::default().fg(t.danger).add_modifier(Modifier::BOLD))), Line::from(s(p.target.display().to_string(), ui::muted(t))), Line::raw("")];
                for (k, l) in &p.diff {
                    let st = match k {
                        '-' => Style::default().fg(t.danger),
                        '+' => Style::default().fg(t.good),
                        _ => ui::muted(t),
                    };
                    v.push(Line::from(s(format!("{k} {l}"), st)));
                }
                v.push(Line::raw(""));
                for n in &p.notes {
                    v.push(Line::from(s(n.clone(), ui::muted(t))));
                }
                ("review the change".into(), v)
            }
        };
        lines.push(Line::raw(""));
        lines.push(Line::from([ui::key_hint("y", "yes", t), ui::key_hint("esc", "no", t)].concat().into_iter().map(|sp| Span::styled(sp.content.into_owned(), sp.style)).collect::<Vec<_>>()));
        self.popup(f, area, &title, lines, self.ask_scroll, t);
    }

    fn popup(&self, f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>, scroll: usize, t: &Theme) {
        let longest = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
        let w = (longest + 4).clamp(60, area.width.saturating_sub(4).max(20));
        let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
        let inner = ui::popup(f, area, w, h, title, t);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        let n = inner.height as usize;
        let scroll = scroll.min(lines.len().saturating_sub(n));
        f.render_widget(Clear, inner);
        f.render_widget(Paragraph::new(lines.into_iter().skip(scroll).take(n).collect::<Vec<_>>()), inner);
    }

    fn draw_note(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let Some(n) = &self.note else { return };
        let mut lines: Vec<Line<'static>> = n.lines.iter().map(|l| Line::from(s(l.clone(), if l.starts_with("  ") { ui::accent(t) } else { Style::default() }))).collect();
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![s("esc", Style::default().fg(t.accent).add_modifier(Modifier::BOLD)), s(" close", ui::muted(t))]));
        self.popup(f, area, &n.title, lines, 0, t);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.ask.is_some() {
            return vec![("y", "yes"), ("esc", "no"), ("↑/↓", "scroll")];
        }
        if self.note.is_some() {
            return vec![("esc", "close")];
        }
        let mut v = match self.view {
            View::Overview => vec![("c", "connect Claude limits"), ("r", "refresh")],
            View::Install => vec![("enter", "install"), ("l", "sign in"), ("o", "docs"), ("r", "recheck")],
            View::Usage => vec![("←/→", "which AI"), ("↑/↓", "scroll days"), ("r", "refresh")],
            View::Saver => vec![("↑/↓", "preset"), ("enter", "apply to Claude"), ("x", "add Codex profile"), ("r", "reload")],
        };
        v.push(("1-4 / tab", "views"));
        v
    }

    fn view_label(v: View) -> (&'static str, &'static str) {
        match v {
            View::Overview => ("gauge", "overview"),
            View::Install => ("package", "install"),
            View::Usage => ("chart", "usage"),
            View::Saver => ("ai", "token saver"),
        }
    }
}

/// Greedy word wrap to `w` columns.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(8);
    let mut out = vec![];
    let mut cur = String::new();
    for word in text.split(' ') {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > w {
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
}

impl Pane for Ais {
    fn title(&self) -> String {
        "your AIs".into()
    }
    fn icon(&self) -> &'static str {
        "gauge"
    }

    fn subtitle(&self) -> Option<String> {
        let (inst, checked) = self.installed_count();
        let today = self.today_all();
        let mut parts = vec![];
        if checked > 0 {
            parts.push(format!("{inst} of {} CLIs installed", CLIS.len()));
        }
        if self.usage.is_some() {
            parts.push(format!("today {} tok · {} API-eq", tok(today.tokens()), money(today.cost)));
        } else if self.scanning {
            parts.push("reading usage logs…".into());
        }
        if let Some(st) = &self.first_stats {
            if st.ms > 0 {
                parts.push(format!("logs read in {:.1}s", st.ms as f64 / 1000.0));
            }
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    fn badge(&self) -> Option<String> {
        let n = now();
        if let Some(w) = self.claude_lim.as_ref().and_then(|l| l.windows.iter().find(|w| w.label == "5-hour")) {
            return Some(format!("5h {:.0}%", if is_reset(w, n) { 0.0 } else { w.pct }));
        }
        let w = self.codex_lim.as_ref()?.first()?;
        Some(format!("cx {:.0}%", if is_reset(w, n) { 0.0 } else { w.pct }))
    }

    fn tick_every(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(5))
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.ensure(cx);
        let t = cx.theme;
        let body = ui::hint_line(f, area, &self.hints(), t);
        let body = Rect { height: body.height.saturating_sub(1), ..body };
        let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
        match self.view {
            View::Overview => self.draw_overview(f, body, t),
            View::Install => self.draw_install(f, body, cx.focused, t),
            View::Usage => self.draw_usage(f, body, t),
            View::Saver => self.draw_saver(f, body, cx.focused, t),
        }
        self.draw_ask(f, area, t);
        self.draw_note(f, area, t);
    }

    fn key(&mut self, key: KeyEvent, cx: &mut Cx) -> bool {
        self.ensure(cx);
        if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) {
            return false;
        }
        if self.ask.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_yes(cx),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.ask = None;
                    cx.notify("cancelled — nothing changed");
                }
                KeyCode::Down | KeyCode::Char('j') => self.ask_scroll += 1,
                KeyCode::Up | KeyCode::Char('k') => self.ask_scroll = self.ask_scroll.saturating_sub(1),
                _ => {}
            }
            return true;
        }
        if self.note.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.note = None;
            }
            return true;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='4') => self.set_view(VIEWS[c as usize - '1' as usize]),
            KeyCode::Tab => self.set_view(VIEWS[(self.view.i() + 1) % VIEWS.len()]),
            KeyCode::BackTab => self.set_view(VIEWS[(self.view.i() + VIEWS.len() - 1) % VIEWS.len()]),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::PageUp => self.step(-10),
            KeyCode::PageDown => self.step(10),
            KeyCode::Home | KeyCode::Char('g') => self.step(-1000),
            KeyCode::End | KeyCode::Char('G') => self.step(1000),
            KeyCode::Char('r') => self.refresh_all(cx),
            KeyCode::Char('c') if self.view == View::Overview => self.plan("connect", cx),
            KeyCode::Enter | KeyCode::Char('i') if self.view == View::Install => self.ask_install(cx),
            KeyCode::Char('l') if self.view == View::Install => self.sign_in(cx),
            KeyCode::Char('o') if self.view == View::Install => {
                let url = CLIS[self.sel[View::Install.i()]].docs;
                self.open_url(url, cx);
            }
            KeyCode::Left | KeyCode::Char('h') if self.view == View::Usage => {
                let n = self.usage_srcs().len().max(1);
                self.usrc = (self.usrc + n - 1) % n;
                self.scroll[View::Usage.i()] = 0;
            }
            KeyCode::Right | KeyCode::Char('l') if self.view == View::Usage => {
                let n = self.usage_srcs().len().max(1);
                self.usrc = (self.usrc + 1) % n;
                self.scroll[View::Usage.i()] = 0;
            }
            KeyCode::Enter | KeyCode::Char('a') if self.view == View::Saver => self.plan("claude", cx),
            KeyCode::Char('x') if self.view == View::Saver => self.plan("codex", cx),
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        match ev.kind {
            MouseEventKind::ScrollDown => self.step(3),
            MouseEventKind::ScrollUp => self.step(-3),
            MouseEventKind::Down(MouseButton::Left) if self.ask.is_none() && self.note.is_none() => {
                let at = Position { x: ev.column, y: ev.row };
                if let Some(&(_, i)) = self.row_hits.iter().find(|(r, _)| r.contains(at)) {
                    let v = self.view.i();
                    self.sel[v] = i;
                    if self.view == View::Saver {
                        self.preset = i;
                    }
                }
                let _ = cx;
            }
            _ => {}
        }
    }

    fn poll(&mut self, cx: &mut Cx) {
        self.ensure(cx);
        self.drain(cx);
        if !self.scanning && self.last_scan.is_none_or(|t| t.elapsed() >= super::REFRESH) {
            self.rescan_usage();
        }
    }

    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.ensure(cx);
        self.side_hits.clear();
        for (k, &v) in VIEWS.iter().enumerate() {
            let y = area.y + k as u16;
            if y >= area.bottom() {
                break;
            }
            let r = Rect { y, height: 1, ..area };
            let (icon, label) = Self::view_label(v);
            let right = match v {
                View::Overview => self.badge().unwrap_or_default(),
                View::Install => {
                    let (i, c) = self.installed_count();
                    if c > 0 { format!("{i}/{}", CLIS.len()) } else { String::new() }
                }
                View::Usage => self.usage.as_ref().map(|_| money(self.today_all().cost)).unwrap_or_default(),
                View::Saver => self.readouts.as_ref().and_then(|r| r.matches).map(|m| m.to_lowercase()).unwrap_or_default(),
            };
            ui::side_row(f, r, icon, label, &right, v == self.view, cx.theme);
            self.side_hits.push((r, v));
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            if let Some(&(_, v)) = self.side_hits.iter().find(|(r, _)| r.contains(Position { x: ev.column, y: ev.row })) {
                self.ensure(cx);
                self.set_view(v);
            }
        }
    }
}
