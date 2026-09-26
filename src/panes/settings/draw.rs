//! Drawing the settings app: the list with its label │ value │ description rows, the panel beside it (the key in
//! config.toml, the default, when a change applies, and what else is worth knowing), and the key hints.

use super::rows::{self, CATS, Ctl, Row};
use super::*;

impl Settings {
    pub(super) fn draw(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        self.take_jump();
        self.start(cx);
        self.pump();
        self.theme_now = cx.theme.name.clone();
        let t = cx.theme;
        let cfg = cx.config;
        self.count_songs(cfg);
        let rows = rows::all(&self.env);
        let lines = self.lines(&rows, cfg);
        let idx = self.index(&lines, &rows);
        let cur_ln = idx.map(|i| lines[i]);
        let cur_row = cur_ln.and_then(|l| match l {
            Ln::Row(i) | Ln::Item(i, _) | Ln::Add(i) => rows.get(i),
            Ln::Head(_) => None,
        });
        let hints = self.hints(cur_row, cur_ln);
        let area = ui::hint_line(f, area, &hints, t);
        let body = Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: area.height.saturating_sub(1) };
        // ---- the heading: the section (or what you're finding), and how many you've changed
        let changed = rows.iter().filter(|r| r.is_setting() && (r.get)(cfg) != rows::all_default(r)).count();
        let head = if self.filtering || !self.filter.is_empty() {
            vec![Span::styled(format!("{}find › ", ui::lead("search")), ui::bold_accent(t)), Span::raw(self.filter.clone()), Span::styled(if self.filtering { "▏" } else { "" }, ui::accent(t))]
        } else {
            vec![Span::styled(format!("{}{}", ui::lead(CATS[self.cat].1), CATS[self.cat].0), ui::bold_accent(t))]
        };
        let mut head = head;
        head.push(Span::styled(if changed > 0 { format!("   ● {changed} changed from the default · saved as you go") } else { "   saved as you go".into() }, ui::muted(t)));
        f.render_widget(Paragraph::new(Line::from(head)), Rect { y: body.y, height: 1, ..body });
        // ---- the list, and the panel beside (or under) it
        let wide = body.width >= 112;
        let list_w = if wide { body.width - (body.width / 3).clamp(44, 60) - 2 } else { body.width };
        let top = body.y + 2;
        let avail_h = body.height.saturating_sub(4);
        let list_h = if wide { avail_h } else { (lines.len() as u16).min(avail_h.saturating_sub(10).max(avail_h / 2)) };
        if let Some(i) = idx {
            let h = list_h.max(1) as usize;
            if i < self.scroll {
                self.scroll = i;
            } else if i >= self.scroll + h {
                self.scroll = i + 1 - h;
            }
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(list_h as usize));
        self.hits.clear();
        if lines.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("  nothing matches: esc clears the search", ui::muted(t))), Rect { x: body.x, y: top, width: list_w, height: 1 });
        }
        let cols = self.columns(&lines, &rows, cfg, list_w as usize);
        let mut drawn: u16 = 0;
        for (row_n, (li, ln)) in lines.iter().enumerate().skip(self.scroll).take(list_h as usize).enumerate() {
            let r = Rect { x: body.x, y: top + row_n as u16, width: list_w, height: 1 };
            self.draw_line(f, r, *ln, &rows, cfg, Some(li) == idx, cols, t);
            if let Some(k) = Self::key_of(*ln, &rows) {
                self.hits.push((r, k));
            }
            drawn += 1;
        }
        // ---- what just happened, or what's wrong with what you typed (under the list)
        let msg_y = top + drawn.max(1) + 1;
        let problem = match &self.edit {
            Some(Edit::Text { err: Some(e), .. }) | Some(Edit::Key { err: Some(e) }) => Some((e.clone(), true)),
            _ => self.msg.clone(),
        };
        if let Some((m, bad)) = problem {
            if msg_y < body.bottom() {
                let st = Style::default().fg(if bad { t.danger } else { t.good });
                f.render_widget(Paragraph::new(Span::styled(ui::fit(&m, list_w as usize), st)), Rect { x: body.x + 2, y: msg_y, width: list_w.saturating_sub(2), height: 1 });
            }
        }
        // ---- the panel
        let pv = if wide {
            Rect { x: body.x + list_w + 2, y: top, width: body.width.saturating_sub(list_w + 2), height: avail_h.min(24) }
        } else {
            let y = msg_y + 2;
            Rect { x: body.x, y, width: body.width, height: body.bottom().saturating_sub(y).min(16) }
        };
        if let (Some(row), Some(ln), true) = (cur_row, cur_ln, pv.height >= 5 && pv.width >= 30) {
            let title = format!("{}{}", ui::lead(CATS[row.cat].1), row.label);
            let inner = ui::frame(f, pv, &title, None, true, t);
            let mut lines = self.detail(row, ln, cfg, t);
            if let Ctl::Choice { opts, .. } = &row.ctl {
                // every choice, the current one lit (like the roster form's agent row)
                let cur = (row.get)(cfg);
                let now = if row.id == "theme" { self.theme_now.clone() } else { cur };
                let mut chips = vec![];
                for (val, lab) in opts.iter().take(24) {
                    let on = *val == now;
                    let st = if on { Style::default().fg(t.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED) } else if row.id == "ai.perms" && val == "bypass" { Style::default().fg(t.danger) } else { ui::muted(t) };
                    chips.push(Span::styled(format!(" {lab} "), st));
                    chips.push(Span::raw(" "));
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(chips));
            }
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner });
        }
    }

    /// The panel beside the list: the setting's key, default and when it applies, and what's worth knowing.
    fn detail(&self, r: &Row, ln: Ln, cfg: &Config, t: &crate::theme::Theme) -> Vec<Line<'static>> {
        let m = ui::muted(t);
        let label = |s: &str| Span::styled(format!("{s:<10}"), m);
        let mut v: Vec<Line> = vec![Line::from(Span::styled(r.desc.clone(), Style::default())), Line::raw("")];
        if r.is_setting() {
            let def = fmt(r, rows::all_default(r), None);
            v.push(Line::from(vec![label("in file"), Span::styled(r.key.clone(), ui::accent(t))]));
            v.push(Line::from(vec![label("default"), Span::raw(def)]));
            v.push(Line::from(vec![label("applies"), Span::raw(r.when.applies().to_string())]));
        }
        let warn = |s: String| Line::from(Span::styled(s, Style::default().fg(t.danger)));
        let good = |s: String| Line::from(Span::styled(s, Style::default().fg(t.good)));
        let note = |s: String| Line::from(Span::styled(s, m));
        let mut extra: Vec<Line> = vec![];
        let val = (r.get)(cfg);
        let g = self.probe.lock().unwrap();
        match r.id.as_str() {
            "theme" if self.preview.is_some() => extra.push(note("enter keeps it · esc goes back".into())),
            "theme" => extra.push(note("the row below opens the theme editor".into())),
            "prefix" => {
                if let Err(e) = crate::config::check_prefix(&val, false) {
                    extra.push(warn(format!("⚠ {e}: ctrl+space is used instead")));
                } else if let Err(e) = crate::config::check_prefix(&val, true) {
                    extra.push(warn(format!("⚠ {e}: that app never gets it")));
                }
                extra.push(note(if matches!(self.edit, Some(Edit::Key { .. })) { "press it now (esc keeps this one)" } else { "enter, then press the new prefix" }.into()));
            }
            "shell" => extra.push(note(format!("the default here: {}", rows::default_shell()))),
            "ai.provider" => match &self.env.avail {
                None => extra.push(note("looking for AIs…".into())),
                Some(av) => {
                    for (id, lab, _) in crate::panes::chat::providers::PROVIDERS {
                        extra.push(if av.contains(id) { good(format!("✓ {lab}")) } else { note(format!("· {lab}: not set up")) });
                    }
                }
            },
            "ai.perms" => {
                if val == "bypass" {
                    extra.push(warn("⚠ bypass: agents never ask and may do anything — only in folders you trust".into()));
                }
                extra.push(note("shift+tab and /perms change the chat you're in; this is where new chats start".into()));
            }
            "ai.effort" => {
                if val == "ultracode" {
                    extra.push(warn("⚠ ultracode runs a whole team of agents: it uses a lot of your plan".into()));
                }
                if let (false, Some(own)) = (val.is_empty(), g.glance.as_ref().and_then(|x| x.claude_own.clone())) {
                    extra.push(warn(format!("⚠ overrides Claude Code's own setting ({own}) in oriel's chats")));
                }
            }
            "ai.models.claude" => match g.glance.as_ref().and_then(|x| x.claude_own.clone()) {
                Some(own) if val.is_empty() => extra.push(note(format!("default = Claude's own: {own}"))),
                Some(own) => extra.push(warn(format!("⚠ overrides Claude's own ({own}) in oriel's chats"))),
                None => extra.push(note("default = Claude Code's own default model".into())),
            },
            "ai.models.codex" => extra.push(note("default = Codex's own (its config.toml)".into())),
            "ai.models.ollama" if g.tags.is_empty() => extra.push(note(format!("default = {} · installed models show up here once Ollama answers", crate::config::AiConfig::default().ollama_model))),
            "ai.models.ollama" => extra.push(note(format!("default = {} · {} installed", crate::config::AiConfig::default().ollama_model, g.tags.len()))),
            "ai.models.openai" => extra.push(note(format!("default = {}", crate::config::AiConfig::default().openai_model))),
            "ai.models.anthropic" => extra.push(note(format!("default = {}", crate::config::AiConfig::default().anthropic_model))),
            "ai.ollama_url" | "ai.openai_url" => {
                let which = if r.id == "ai.ollama_url" { "ollama" } else { "openai" };
                match g.tests.get(which) {
                    Some(Ok(s)) => extra.push(good(format!("✓ {s}"))),
                    Some(Err(e)) => extra.push(warn(format!("✗ {e}"))),
                    None if self.tests_running.contains(which) => extra.push(note("testing…".into())),
                    None => extra.push(note("t tests it".into())),
                }
            }
            "ai.openai_key" | "ai.anthropic_key" => {
                let env = if r.id == "ai.openai_key" { "OPENAI_API_KEY" } else { "ANTHROPIC_API_KEY" };
                let in_env = std::env::var(env).is_ok_and(|e| !e.is_empty());
                match (val.is_empty(), in_env) {
                    (true, true) => extra.push(good(format!("using ${env} from the environment"))),
                    (false, true) => extra.push(note(format!("this one wins over ${env}"))),
                    _ => {}
                }
                extra.push(note("kept as plain text in config.toml; an environment variable keeps it out of the file".into()));
                extra.push(note("enter types a new one (out of sight) · x clears it".into()));
            }
            "lead.protocol" => extra.push(note("auto: MCP tools when the lead's CLI can load them (claude, codex, kimi), else actions written as text".into())),
            "lead.gate" | "lead.gates.here" => match &self.env.repo {
                Some((root, detected)) => {
                    let name = std::path::Path::new(root).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    let gate = crate::config::gate_for(&cfg.lead, std::path::Path::new(root));
                    extra.push(note(match gate.trim() {
                        "" => format!("for {name}: {} (detected)", detected.clone().unwrap_or_else(|| "nothing found to run".into())),
                        "none" | "off" => format!("for {name}: no check"),
                        g => format!("for {name}: {g}"),
                    }));
                }
                None => extra.push(note("detect: Cargo → cargo check · go.mod → go build · package.json with a build script → install + build".into())),
            },
            "lead.gate_timeout_s" => extra.push(note("at least 30 s".into())),
            "roster.edit" => {
                if cfg.roster.is_empty() {
                    extra.push(note("using defaults from what's installed: any edit makes it your own".into()));
                }
                for w in &cfg.roster {
                    extra.push(note(format!("{} {} · {} · {}{}", if w.enabled { "●" } else { "○" }, w.name, w.agent, if w.model.is_empty() { "default" } else { &w.model }, format!(" · {}", w.tier))));
                }
            }
            "tools.config" => extra.push(note("c copies the path".into())),
            "notes_folder" => {
                let dir = crate::config::notes_dir(cfg);
                extra.push(note(dir.display().to_string()));
                extra.push(if dir.is_dir() { good("exists ✓".into()) } else { warn("doesn't exist yet: made with the first note".into()) });
            }
            "music.folders" => match ln {
                Ln::Item(_, k) => {
                    let f = val.lines().nth(k).unwrap_or("").to_string();
                    extra.push(match g.songs.get(&f) {
                        Some(Some(n)) => good(format!("✓ {}", songs(*n))),
                        Some(None) => warn("that folder isn't there".into()),
                        None => note("counting songs…".into()),
                    });
                    extra.push(note("x removes it · J/K move it · enter changes it".into()));
                }
                _ => extra.push(note("a adds a folder · x removes one · J/K reorder".into())),
            },
            "music.source" => {
                if crate::panes::music::audio_player_library() {
                    extra.push(note("the audio-player library is on this computer".into()));
                } else {
                    extra.push(note("no audio-player library here: the folders are used either way".into()));
                }
            }
            _ if r.id.starts_with("alerts.desktop.") && !cfg.desktop_notifications => extra.push(warn("desktop notifications are off (the first row)".into())),
            _ => {}
        }
        if !extra.is_empty() {
            v.push(Line::raw(""));
            v.extend(extra);
        }
        v
    }

    fn hints(&self, row: Option<&Row>, ln: Option<Ln>) -> Vec<(&'static str, &'static str)> {
        match &self.edit {
            Some(Edit::Key { .. }) => return vec![("press", "the new prefix: ctrl+<letter> or ctrl+space"), ("esc", "cancel")],
            Some(Edit::Text { item, id, .. }) => {
                let what = if item.is_some() || id == "notes_folder" {
                    "a folder"
                } else if row.is_some_and(|r| matches!(r.ctl, Ctl::Secret { .. })) {
                    "the key (it stays hidden)"
                } else {
                    "a value"
                };
                let mut h = vec![("type", what)];
                if what == "a folder" {
                    h.push(("tab", "complete"));
                }
                h.extend([("enter", "save"), ("esc", "cancel")]);
                return h;
            }
            None => {}
        }
        if self.filtering {
            return vec![("type", "to find a setting"), ("enter", "done"), ("esc", "clear")];
        }
        let mut h = vec![("↑↓", "setting"), ("tab", "section")];
        match (row.map(|r| &r.ctl), ln) {
            (Some(_), Some(Ln::Item(..))) => h.extend([("x", "remove"), ("J/K", "move"), ("a", "add")]),
            (Some(_), Some(Ln::Add(_))) => h.push(("enter", "add a folder")),
            (Some(Ctl::Toggle), _) => h.push(("space", "switch")),
            (Some(Ctl::Choice { custom: true, .. }), _) => h.extend([("←→", "choose"), ("enter", "your own")]),
            (Some(Ctl::Choice { .. }), _) => h.push(("←→", "choose")),
            (Some(Ctl::Number { .. }), _) => h.extend([("←→", "change"), ("enter", "type it")]),
            (Some(Ctl::Text | Ctl::Folder), _) => h.push(("enter", "edit")),
            (Some(Ctl::Secret { .. }), _) => h.extend([("enter", "new key"), ("x", "clear")]),
            (Some(Ctl::List), _) => h.push(("a", "add a folder")),
            (Some(Ctl::Key), _) => h.push(("enter", "press a new one")),
            (Some(Ctl::Link(_)), _) => h.push(("enter", "go there")),
            (Some(Ctl::Act(_)), _) => h.push(("enter", "do it")),
            _ => {}
        }
        if row.is_some_and(|r| r.is_setting()) {
            h.push(("r", "default"));
        }
        h.extend([("/", "find"), ("o", "config.toml")]);
        h
    }

    /// The label and value columns: as wide as this list needs, within reason.
    fn columns(&self, lines: &[Ln], rows: &[Row], cfg: &Config, w: usize) -> (usize, usize) {
        let width = |s: &str| unicode_width::UnicodeWidthStr::width(s);
        let row_ids: Vec<usize> = lines.iter().filter_map(|l| if let Ln::Row(i) = l { Some(*i) } else { None }).collect();
        let lw = row_ids.iter().map(|&i| width(&rows[i].label)).max().unwrap_or(12).clamp(12, 26);
        let vw = row_ids.iter().map(|&i| width(&shown(&rows[i], cfg, Some(self))) + 4).max().unwrap_or(12).clamp(12, 34);
        (lw, vw.min(w.saturating_sub(lw + 7) / 2).max(10))
    }

    fn draw_line(&self, f: &mut Frame, r: Rect, ln: Ln, rows: &[Row], cfg: &Config, on: bool, cols: (usize, usize), t: &crate::theme::Theme) {
        let w = r.width as usize;
        let mark = Span::styled(if on { "▸ " } else { "  " }, ui::bold_accent(t));
        let line = match ln {
            Ln::Head(c) => Line::from(Span::styled(format!("── {}", CATS[c].0), ui::muted(t))),
            Ln::Row(i) => {
                let row = &rows[i];
                let changed = row.is_setting() && (row.get)(cfg) != rows::all_default(row);
                let (lw, vw) = cols;
                let name_st = if on { ui::bold_accent(t) } else { Style::default() };
                let editing = match &self.edit {
                    Some(Edit::Text { id, input, item: None, .. }) if *id == row.id => Some(if matches!(row.ctl, Ctl::Secret { .. }) { mask_typing(&input.text) } else { input.text.clone() }),
                    Some(Edit::Key { .. }) if row.id == "prefix" => Some("press a key…".into()),
                    _ => None,
                };
                let mut value = editing.clone().unwrap_or_else(|| shown(row, cfg, Some(self)));
                let choice = matches!(row.ctl, Ctl::Choice { .. } | Ctl::Number { .. });
                if on && choice && editing.is_none() {
                    value = format!("‹ {value} ›");
                }
                let val_st = if editing.is_some() {
                    Style::default().fg(t.accent).add_modifier(Modifier::UNDERLINED)
                } else if row.id == "ai.perms" && (row.get)(cfg) == "bypass" {
                    Style::default().fg(t.danger).add_modifier(Modifier::BOLD)
                } else if matches!(row.ctl, Ctl::Link(_) | Ctl::Act(_)) {
                    ui::muted(t)
                } else if changed {
                    Style::default().fg(t.shine)
                } else {
                    Style::default()
                };
                let cursor = if editing.is_some() && matches!(self.edit, Some(Edit::Text { .. })) { "▏" } else { "" };
                let room = vw.saturating_sub(cursor.chars().count());
                // a path (or what you're typing) keeps its end: that's the part that differs
                let value = if matches!(row.ctl, Ctl::Folder) || editing.is_some() || row.id == "tools.config" { fit_left(&value, room) } else { ui::fit(&value, room) };
                let value = format!("{value}{cursor}");
                let mut spans = vec![
                    mark,
                    Span::styled(if changed { "● " } else { "  " }, ui::accent(t)),
                    Span::styled(format!("{:<lw$}", ui::fit(&row.label, lw)), name_st),
                    Span::styled(" │ ", Style::default().fg(t.frame)),
                    Span::styled(format!("{value:<vw$}"), val_st),
                ];
                let used = 4 + lw + 3 + vw;
                if w > used + 12 {
                    spans.push(Span::styled(" │ ", Style::default().fg(t.frame)));
                    spans.push(Span::styled(ui::fit(&row.desc, w - used - 3), ui::muted(t)));
                }
                Line::from(spans)
            }
            Ln::Item(i, k) => {
                let val = (rows[i].get)(cfg);
                let folder = val.lines().nth(k).unwrap_or("").to_string();
                let editing = match &self.edit {
                    Some(Edit::Text { id, input, item: Some(Some(j)), .. }) if *id == rows[i].id && *j == k => Some(format!("{}▏", input.text)),
                    _ => None,
                };
                let count = match self.probe.lock().unwrap().songs.get(&folder) {
                    Some(Some(n)) => songs(*n),
                    Some(None) => "not there".into(),
                    None => "counting…".into(),
                };
                let st = if on { ui::bold_accent(t) } else { Style::default() };
                Line::from(vec![
                    mark,
                    Span::styled(format!("    {} ", if k + 1 < val.lines().count() { "├" } else { "└" }), Style::default().fg(t.frame)),
                    Span::styled(fit_left(editing.as_deref().unwrap_or(&folder), w.saturating_sub(22)), st),
                    Span::styled(format!("  {count}"), ui::muted(t)),
                ])
            }
            Ln::Add(i) => {
                let editing = match &self.edit {
                    Some(Edit::Text { id, input, item: Some(None), .. }) if *id == rows[i].id => Some(format!("{}▏", input.text)),
                    _ => None,
                };
                let st = if on { ui::bold_accent(t) } else { ui::muted(t) };
                Line::from(vec![mark, Span::styled("      + ", ui::accent(t)), Span::styled(editing.unwrap_or_else(|| "add a folder".into()), st)])
            }
        };
        f.render_widget(Paragraph::new(line), r);
    }
}

/// "1 song", "240 songs", "20000+ songs" (the count stops there).
fn songs(n: usize) -> String {
    match n {
        1 => "1 song".into(),
        n if n >= 20_000 => format!("{n}+ songs"),
        n => format!("{n} songs"),
    }
}

/// Truncate to `w` columns keeping the end ("…\music\albums").
fn fit_left(s: &str, w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= w {
        return s.to_string();
    }
    let mut out: Vec<char> = vec![];
    let mut used = 1;
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    std::iter::once('…').chain(out.into_iter().rev()).collect()
}

/// A key being typed: all dots but the last four characters.
fn mask_typing(s: &str) -> String {
    let n = s.chars().count();
    let keep = n.saturating_sub(4);
    format!("{}{}", "•".repeat(keep), s.chars().skip(keep).collect::<String>())
}

