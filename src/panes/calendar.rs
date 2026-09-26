//! calendar: a month at a glance, the chosen day's plans beside it, and a reminder toast when a timed plan comes
//! up (from any app). Plans live in <data dir>/calendar.json. `a` adds one: "2pm dentist", "10:30 standup".

use crate::pane::{Cx, Pane};
use crate::panes::files::clock;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Plan {
    /// days since 1970-01-01, local calendar
    pub day: i64,
    /// minutes after midnight, if it has a time
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u32>,
    pub text: String,
}

// ------------------------------------------------------------------ dates (no chrono: the civil-date formulas)
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

/// 0 = Monday … 6 = Sunday.
pub fn weekday(day: i64) -> u32 {
    (day + 3).rem_euclid(7) as u32
}

fn days_in_month(y: i64, m: u32) -> u32 {
    let next = if m == 12 { days_from_civil(y + 1, 1, 1) } else { days_from_civil(y, m + 1, 1) };
    (next - days_from_civil(y, m, 1)) as u32
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/// (today, minutes since midnight), local time.
fn now() -> (i64, u32) {
    let l = clock::local(clock::now_secs());
    (days_from_civil(l.year as i64, l.month, l.day), l.hour * 60 + l.min)
}

fn hhmm(m: u32) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// A time at the start of what you typed: "14:30 x", "2pm x", "2:30pm x", "9am x". Returns it and the rest.
pub fn parse_time(s: &str) -> (Option<u32>, String) {
    let s = s.trim();
    let (first, rest) = s.split_once(' ').unwrap_or((s, ""));
    let f = first.to_ascii_lowercase();
    let (num, pm, am) = if let Some(x) = f.strip_suffix("pm") {
        (x, true, false)
    } else if let Some(x) = f.strip_suffix("am") {
        (x, false, true)
    } else {
        (f.as_str(), false, false)
    };
    let (h, m) = match num.split_once(':').or_else(|| num.split_once('.')) {
        Some((h, m)) => (h.parse::<u32>().ok(), m.parse::<u32>().ok()),
        // a bare number only counts as a time with am/pm ("3 apples" is not 3 o'clock)
        None if pm || am => (num.parse::<u32>().ok(), Some(0)),
        None => (None, None),
    };
    match (h, m) {
        (Some(mut h), Some(m)) if h <= 24 && m < 60 && !rest.trim().is_empty() => {
            if pm && h < 12 {
                h += 12;
            }
            if am && h == 12 {
                h = 0;
            }
            (Some((h % 24) * 60 + m), rest.trim().to_string())
        }
        _ => (None, s.to_string()),
    }
}

enum Input {
    Add(String),
    Edit(usize, String),
}

pub struct Calendar {
    path: PathBuf,
    plans: Vec<Plan>,
    sel: i64,
    today: i64,
    /// which of the chosen day's plans is picked (for e / x)
    pick: usize,
    input: Option<Input>,
    undo: Option<Plan>,
    reminded: HashSet<(i64, u32, String)>,
    cells: Vec<(Rect, i64)>,
    side_hits: Vec<(Rect, i64)>,
}

pub fn store_path() -> PathBuf {
    if cfg!(test) {
        return std::path::absolute("target/test-scratch/calendar.json").unwrap_or_default();
    }
    crate::config::data_dir().join("calendar.json")
}

/// Plans still to come today or later with a time: worth keeping a calendar open for its reminders.
pub fn has_timed_plans() -> bool {
    let (today, _) = now();
    load(&store_path()).iter().any(|p| p.day >= today && p.at.is_some())
}

fn load(path: &std::path::Path) -> Vec<Plan> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

impl Calendar {
    pub fn new() -> Calendar {
        Calendar::open_at(store_path())
    }

    pub fn open_at(path: PathBuf) -> Calendar {
        let (today, _) = now();
        let mut c = Calendar { plans: load(&path), path, sel: today, today, pick: 0, input: None, undo: None, reminded: HashSet::new(), cells: vec![], side_hits: vec![] };
        c.sort();
        c
    }

    fn sort(&mut self) {
        self.plans.sort_by(|a, b| (a.day, a.at.unwrap_or(0), &a.text).cmp(&(b.day, b.at.unwrap_or(0), &b.text)));
    }

    fn save(&mut self) {
        self.sort();
        if let Some(d) = self.path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&self.path, serde_json::to_string_pretty(&self.plans).unwrap_or_default());
    }

    /// Indices of a day's plans, in time order.
    fn on(&self, day: i64) -> Vec<usize> {
        (0..self.plans.len()).filter(|&i| self.plans[i].day == day).collect()
    }

    fn go(&mut self, day: i64) {
        self.sel = day;
        self.pick = 0;
    }

    fn shift_month(&mut self, by: i32) {
        let (y, m, d) = civil_from_days(self.sel);
        let idx = y * 12 + m as i64 - 1 + by as i64;
        let (ny, nm) = (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32);
        self.go(days_from_civil(ny, nm, d.min(days_in_month(ny, nm))));
    }

    fn plan_line(&self, p: &Plan, width: usize, t: &crate::theme::Theme) -> Line<'static> {
        let time = p.at.map(hhmm).unwrap_or_else(|| "all day".into());
        let past = p.day < self.today || (p.day == self.today && p.at.is_some_and(|a| a + 60 < now().1));
        let text_style = if past { Style::default().fg(t.muted) } else { Style::default() };
        Line::from(vec![Span::styled(format!("{time:<8}"), ui::accent(t)), Span::styled(ui::fit(&p.text, width.saturating_sub(9)), text_style)])
    }
}

impl Pane for Calendar {
    fn title(&self) -> String {
        let (y, m, _) = civil_from_days(self.sel);
        format!("calendar · {} {y}", MONTHS[m as usize - 1])
    }
    fn icon(&self) -> &'static str {
        "calendar"
    }
    fn badge(&self) -> Option<String> {
        let n = self.on(self.today).len();
        (n > 0).then(|| format!("{n} today"))
    }
    fn tick_every(&self) -> Option<Duration> {
        Some(Duration::from_secs(15))
    }
    fn ticks_hidden(&self) -> bool {
        true
    }

    /// Reminders: a toast when a timed plan starts (and the date rolls over at midnight).
    fn poll(&mut self, cx: &mut Cx) {
        let (today, mins) = now();
        if today != self.today {
            if self.sel == self.today {
                self.sel = today;
            }
            self.today = today;
        }
        for p in &self.plans {
            if let (true, Some(at)) = (p.day == today, p.at) {
                // ten minutes before, and when it starts
                let soon = (p.day, at + 10_000, p.text.clone());
                if mins + 10 >= at && mins < at && !self.reminded.contains(&soon) {
                    cx.alert(crate::alerts::Kind::Calendar, format!("in {} min: {} {}", at - mins, hhmm(at), p.text));
                    self.reminded.insert(soon);
                }
                let key = (p.day, at, p.text.clone());
                if mins >= at && mins < at + 10 && !self.reminded.contains(&key) {
                    cx.alert(crate::alerts::Kind::Calendar, format!("now: {} {}", hhmm(at), p.text));
                    self.reminded.insert(key);
                }
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        let hints: &[(&str, &str)] = match self.input {
            Some(_) => &[("type", "a plan, a time first if it has one: 2pm dentist"), ("enter", "save"), ("esc", "cancel")],
            None => &[("← → ↑ ↓", "day"), ("[ ]", "month"), ("t", "today"), ("a", "add"), ("j k", "pick"), ("e", "edit"), ("x", "delete")],
        };
        let area = ui::hint_line(f, area, hints, t);
        let body = Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: area.height };
        let wide = body.width >= 100;
        let grid_w = if wide { (body.width - 36).min(7 * 18) } else { body.width.min(7 * 18) };
        let (y0, m0, _) = civil_from_days(self.sel);
        // ---- the month's name
        let head = Line::from(Span::styled(format!("{} {y0}", MONTHS[m0 as usize - 1]), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)));
        f.render_widget(Paragraph::new(head), Rect { x: body.x, y: body.y + 1, width: grid_w, height: 1 });
        // ---- weekday names
        let cw = (grid_w / 7).max(4);
        let wy = body.y + 3;
        for (i, d) in DAYS.iter().enumerate() {
            let label = if cw >= 10 { &d[..3] } else { &d[..2] };
            let st = if i >= 5 { ui::muted(t) } else { Style::default().fg(t.shine) };
            f.render_widget(Paragraph::new(Span::styled(format!(" {label}"), st)), Rect { x: body.x + i as u16 * cw, y: wy, width: cw, height: 1 });
        }
        // ---- the weeks: 6 rows, each cell the day number and what's on
        let first = days_from_civil(y0, m0, 1);
        let start = first - weekday(first) as i64;
        let rows_h = body.bottom().saturating_sub(wy + 1);
        let ch = (rows_h / 6).clamp(1, 5);
        let tint = |c: Color, a: f32| match c {
            Color::Rgb(..) => Some(crate::theme::mix(Color::Rgb(14, 14, 17), c, a)),
            _ => None,
        };
        self.cells.clear();
        for w in 0..6u16 {
            for d in 0..7u16 {
                let day = start + (w * 7 + d) as i64;
                let r = Rect { x: body.x + d * cw, y: wy + 1 + w * ch, width: cw.saturating_sub(1), height: ch };
                if r.bottom() > body.bottom() {
                    continue;
                }
                let (_, m, dn) = civil_from_days(day);
                let inside = m == m0;
                let on = self.on(day);
                let selected = day == self.sel;
                if selected {
                    if let Some(bg) = tint(t.accent, 0.22) {
                        f.render_widget(ratatui::widgets::Block::default().style(Style::default().bg(bg)), r);
                    }
                }
                let num_style = if day == self.today {
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else if !inside {
                    Style::default().fg(t.frame)
                } else if selected {
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
                } else if d >= 5 {
                    ui::muted(t)
                } else {
                    Style::default()
                };
                let mut top = vec![Span::styled(format!("{dn:>2}"), num_style)];
                if !on.is_empty() && (ch == 1 || cw < 9) {
                    top.push(Span::styled(" •", Style::default().fg(t.shine)));
                }
                f.render_widget(Paragraph::new(Line::from(top)), Rect { height: 1, x: r.x + 1, width: r.width.saturating_sub(1), ..r });
                // what's on, in the space left
                for (k, &i) in on.iter().take(ch.saturating_sub(1) as usize).enumerate() {
                    let last = k + 2 == ch as usize && on.len() > k + 1;
                    let label = if last && on.len() > ch as usize - 1 { format!("+{} more", on.len() - k) } else { self.plans[i].text.clone() };
                    let st = if inside { Style::default().fg(t.shine) } else { Style::default().fg(t.frame) };
                    f.render_widget(Paragraph::new(Span::styled(ui::fit(&label, r.width.saturating_sub(2) as usize), st)), Rect { x: r.x + 1, y: r.y + 1 + k as u16, width: r.width.saturating_sub(1), height: 1 });
                }
                self.cells.push((r, day));
            }
        }
        // ---- the chosen day
        let (ay, aw) = if wide { (body.y + 1, body.width.saturating_sub(grid_w + 3)) } else { (wy + 1 + 6 * ch + 1, body.width) };
        let ax = if wide { body.x + grid_w + 3 } else { body.x };
        if aw >= 20 && ay + 3 < body.bottom() {
            let (_, m, d) = civil_from_days(self.sel);
            let when = match self.sel - self.today {
                0 => "today".to_string(),
                1 => "tomorrow".into(),
                -1 => "yesterday".into(),
                n if n > 1 => format!("in {n} days"),
                n => format!("{} days ago", -n),
            };
            let mut lines = vec![
                Line::from(vec![Span::styled(format!("{} {d} {}", DAYS[weekday(self.sel) as usize], MONTHS[m as usize - 1]), ui::bold_accent(t)), Span::styled(format!("  {when}"), ui::muted(t))]),
                Line::raw(""),
            ];
            let on = self.on(self.sel);
            if on.is_empty() {
                lines.push(Line::from(Span::styled("nothing planned · a adds something", ui::muted(t))));
            }
            for (k, &i) in on.iter().enumerate() {
                let mut l = self.plan_line(&self.plans[i], aw as usize - 2, t);
                if k == self.pick.min(on.len() - 1) {
                    l.spans.insert(0, Span::styled("▸ ", ui::bold_accent(t)));
                } else {
                    l.spans.insert(0, Span::raw("  "));
                }
                lines.push(l);
            }
            match &self.input {
                Some(Input::Add(s)) | Some(Input::Edit(_, s)) => {
                    lines.push(Line::raw(""));
                    let what = if matches!(self.input, Some(Input::Add(_))) { "add › " } else { "edit › " };
                    lines.push(Line::from(vec![Span::styled(what, ui::bold_accent(t)), Span::raw(s.clone()), Span::styled("▏", ui::accent(t))]));
                }
                None => {
                    if self.undo.is_some() {
                        lines.push(Line::raw(""));
                        lines.push(Line::from(Span::styled("deleted · u puts it back", ui::muted(t))));
                    }
                }
            }
            f.render_widget(Paragraph::new(lines), Rect { x: ax, y: ay, width: aw, height: body.bottom().saturating_sub(ay) });
        }
    }

    fn key(&mut self, k: KeyEvent, _cx: &mut Cx) -> bool {
        if let Some(input) = &mut self.input {
            let buf = match input {
                Input::Add(s) | Input::Edit(_, s) => s,
            };
            match k.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => buf.push(c),
                KeyCode::Enter => {
                    match self.input.take() {
                        Some(Input::Add(s)) if !s.trim().is_empty() => {
                            let (at, text) = parse_time(&s);
                            self.plans.push(Plan { day: self.sel, at, text });
                        }
                        Some(Input::Edit(i, s)) if !s.trim().is_empty() && i < self.plans.len() => {
                            let (at, text) = parse_time(&s);
                            self.plans[i].at = at;
                            self.plans[i].text = text;
                        }
                        _ => {}
                    }
                    self.save();
                }
                _ => {}
            }
            return true;
        }
        let on = self.on(self.sel);
        let picked = on.get(self.pick.min(on.len().saturating_sub(1))).copied();
        match k.code {
            KeyCode::Left | KeyCode::Char('h') => self.go(self.sel - 1),
            KeyCode::Right | KeyCode::Char('l') => self.go(self.sel + 1),
            KeyCode::Up => self.go(self.sel - 7),
            KeyCode::Down => self.go(self.sel + 7),
            KeyCode::Char('[') | KeyCode::PageUp => self.shift_month(-1),
            KeyCode::Char(']') | KeyCode::PageDown => self.shift_month(1),
            KeyCode::Char('t') | KeyCode::Home => self.go(self.today),
            KeyCode::Char('a') | KeyCode::Enter => self.input = Some(Input::Add(String::new())),
            KeyCode::Char('j') => self.pick = (self.pick + 1).min(on.len().saturating_sub(1)),
            KeyCode::Char('k') => self.pick = self.pick.saturating_sub(1),
            KeyCode::Char('e') => {
                if let Some(i) = picked {
                    let p = &self.plans[i];
                    let s = match p.at {
                        Some(a) => format!("{} {}", hhmm(a), p.text),
                        None => p.text.clone(),
                    };
                    self.input = Some(Input::Edit(i, s));
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(i) = picked {
                    self.undo = Some(self.plans.remove(i));
                    self.pick = self.pick.saturating_sub(1);
                    self.save();
                }
            }
            KeyCode::Char('u') => {
                if let Some(p) = self.undo.take() {
                    self.go(p.day);
                    self.plans.push(p);
                    self.save();
                }
            }
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position { x: ev.column, y: ev.row };
                if let Some(&(_, day)) = self.cells.iter().find(|(r, _)| r.contains(pos)) {
                    self.go(day);
                }
            }
            MouseEventKind::ScrollDown => self.shift_month(1),
            MouseEventKind::ScrollUp => self.shift_month(-1),
            _ => {}
        }
    }

    /// Coming up: the next two weeks.
    fn side(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        let t = cx.theme;
        self.side_hits.clear();
        let mut y = area.y;
        f.render_widget(Paragraph::new(Span::styled("coming up", ui::muted(t))), Rect { y, height: 1, ..area });
        y += 1;
        let soon: Vec<&Plan> = self.plans.iter().filter(|p| p.day >= self.today && p.day < self.today + 14).collect();
        if soon.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("nothing in the next two weeks", ui::muted(t))), Rect { y, height: 1, ..area });
        }
        for p in soon {
            if y >= area.bottom() {
                break;
            }
            let (_, _, d) = civil_from_days(p.day);
            let day = if p.day == self.today { "today".to_string() } else if p.day == self.today + 1 { "tmrw".into() } else { format!("{} {d}", &DAYS[weekday(p.day) as usize][..3]) };
            let time = p.at.map(hhmm).unwrap_or_default();
            let r = Rect { y, height: 1, ..area };
            let line = Line::from(vec![
                Span::styled(format!("{day:<7}"), Style::default().fg(if p.day == self.today { t.accent } else { t.shine })),
                Span::styled(format!("{time:<6}"), ui::muted(t)),
                Span::raw(ui::fit(&p.text, (area.width as usize).saturating_sub(13))),
            ]);
            f.render_widget(Paragraph::new(line), r);
            self.side_hits.push((r, p.day));
            y += 1;
        }
    }

    fn side_mouse(&mut self, ev: MouseEvent, _area: Rect, _cx: &mut Cx) {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let pos = Position { x: ev.column, y: ev.row };
            if let Some(&(_, day)) = self.side_hits.iter().find(|(r, _)| r.contains(pos)) {
                self.go(day);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Kit;

    #[test]
    fn calendar_dates_and_times() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2026, 9, 25)), (2026, 9, 25));
        assert_eq!(weekday(days_from_civil(2026, 9, 25)), 4, "a Friday");
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(parse_time("2pm dentist"), (Some(14 * 60), "dentist".into()));
        assert_eq!(parse_time("10:30 standup"), (Some(630), "standup".into()));
        assert_eq!(parse_time("12am midnight snack"), (Some(0), "midnight snack".into()));
        assert_eq!(parse_time("3 apples"), (None, "3 apples".into()), "a bare number isn't a time");
        assert_eq!(parse_time("buy milk"), (None, "buy milk".into()));
    }

    #[test]
    fn calendar_add_edit_delete() {
        let path = std::path::absolute("target/test-scratch/calendar-test.json").unwrap();
        let _ = std::fs::remove_file(&path);
        let mut k = Kit::new();
        let mut c = Calendar::open_at(path.clone());
        c.today = days_from_civil(2026, 9, 25);
        c.sel = c.today;
        k.key(&mut c, KeyCode::Char('a'));
        k.typ(&mut c, "2pm dentist");
        k.key(&mut c, KeyCode::Enter);
        k.key(&mut c, KeyCode::Char('a'));
        k.typ(&mut c, "call mom");
        k.key(&mut c, KeyCode::Enter);
        k.key(&mut c, KeyCode::Right); // tomorrow
        k.key(&mut c, KeyCode::Char('a'));
        k.typ(&mut c, "10:30 standup");
        k.key(&mut c, KeyCode::Enter);
        k.key(&mut c, KeyCode::Char('t'));
        assert_eq!(c.plans.len(), 3);
        assert_eq!(load(&path).len(), 3, "saved");
        let s = k.render_html(&mut c, 140, 40, "target/snap/calendar.html");
        assert!(s.contains("September 2026") && s.contains("dentist") && s.contains("14:00") && s.contains("today"), "{s}");
        assert_eq!(c.badge().as_deref(), Some("2 today"));
        let side = k.render_side(&mut c, 34, 12);
        assert!(side.contains("coming up") && side.contains("standup") && side.contains("tmrw"), "{side}");
        // edit the picked one (all-day first), then delete and undo
        k.key(&mut c, KeyCode::Char('e'));
        for _ in 0..20 {
            k.key(&mut c, KeyCode::Backspace);
        }
        k.typ(&mut c, "6pm call mom");
        k.key(&mut c, KeyCode::Enter);
        assert!(c.plans.iter().any(|p| p.text == "call mom" && p.at == Some(18 * 60)));
        k.key(&mut c, KeyCode::Char('x'));
        assert_eq!(c.plans.len(), 2);
        k.key(&mut c, KeyCode::Char('u'));
        assert_eq!(c.plans.len(), 3);
        // months roll over
        k.key(&mut c, KeyCode::Char(']'));
        assert_eq!(civil_from_days(c.sel).1, 10);
    }
}
