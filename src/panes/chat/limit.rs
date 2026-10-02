//! An AI stopped because you hit your plan's usage limit: when does it reset? Read from what the CLI said, so the
//! chat can say "continuing automatically at 06:00" and carry on by itself then.
//!
//! Claude Code says "You've hit your limit · resets 6am (Europe/Berlin)" or "resets Oct 2, 6am" (older versions:
//! "Claude AI usage limit reached|1759370400"); Codex says "try again at 3:05 PM" or "try again in 2 hours 5 minutes".

use crate::panes::files::clock;

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// When the limit `text` talks about resets (unix seconds), or None if it isn't a usage limit or says no time.
pub fn reset_at(text: &str, now: i64) -> Option<i64> {
    let low = text.to_lowercase();
    if !low.contains("limit") {
        return None;
    }
    // "…limit reached|1759370400"
    if let Some(i) = low.find('|') {
        let digits: String = low[i + 1..].chars().take_while(char::is_ascii_digit).collect();
        if digits.len() >= 9 {
            return digits.parse().ok();
        }
    }
    // "try again in 2 days 3 hours 4 minutes"
    if let Some(i) = low.find("try again in ") {
        let secs = duration(&low[i + "try again in ".len()..]);
        if secs > 0 {
            return Some(now + secs);
        }
    }
    for key in ["resets at ", "resets ", "reset at ", "try again at ", "available again at "] {
        if let Some(i) = low.find(key) {
            if let Some(t) = when(&low[i + key.len()..], now) {
                return Some(t);
            }
        }
    }
    None
}

/// "2 days 3 hours 4 minutes" (or 2h 5m) in seconds.
fn duration(s: &str) -> i64 {
    let mut total = 0;
    let mut num: Option<i64> = None;
    let words: Vec<&str> = s.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).take(12).collect();
    for w in words {
        let digits: String = w.chars().take_while(char::is_ascii_digit).collect();
        let rest = &w[digits.len()..];
        if !digits.is_empty() {
            num = digits.parse().ok();
        }
        let unit = rest.trim_matches(|c: char| !c.is_alphabetic());
        if unit.is_empty() {
            continue;
        }
        let Some(n) = num.take() else { break };
        total += n * match unit.chars().next() {
            Some('d') => 86_400,
            Some('h') => 3600,
            Some('m') if !unit.starts_with("mo") => 60,
            Some('s') => 1,
            _ => break,
        };
    }
    total
}

/// "6am", "6:30 pm", "18:00", "oct 2, 6am", "october 4th, 2026 3:05 pm" → the next such moment from `now`.
fn when(s: &str, now: i64) -> Option<i64> {
    let s = s.trim_start();
    let mut rest = s;
    let mut date: Option<(Option<i32>, u32, u32)> = None;
    if let Some(m) = MONTHS.iter().position(|m| rest.starts_with(m)) {
        let after = rest.trim_start_matches(|c: char| c.is_alphabetic()).trim_start();
        let day: String = after.chars().take_while(char::is_ascii_digit).collect();
        let d: u32 = day.parse().ok().filter(|d| (1..=31).contains(d))?;
        let mut tail = after[day.len()..].trim_start_matches(|c: char| c.is_alphabetic()).trim_start_matches([',', ' ']);
        let year: String = tail.chars().take_while(char::is_ascii_digit).collect();
        let y = (year.len() == 4).then(|| year.parse().ok()).flatten();
        if y.is_some() {
            tail = tail[4..].trim_start_matches([',', ' ']);
        }
        tail = tail.trim_start_matches("at ").trim_start();
        date = Some((y, m as u32 + 1, d));
        rest = tail;
    }
    let (h, min) = clock_time(rest)?;
    let today = clock::local(now);
    let now_min = (today.hour * 60 + today.min) as i64;
    let at_today = now + (h as i64 * 60 + min as i64 - now_min) * 60 - today.sec as i64;
    Some(match date {
        None => {
            if at_today <= now { at_today + 86_400 } else { at_today }
        }
        Some((y, m, d)) => {
            let day_of = |y: i64, m: u32, d: u32| crate::panes::calendar::days_from_civil(y, m, d);
            let base = day_of(today.year as i64, today.month, today.day);
            let mut target = day_of(y.unwrap_or(today.year) as i64, m, d);
            if y.is_none() && target < base {
                target = day_of(today.year as i64 + 1, m, d);
            }
            at_today + (target - base) * 86_400
        }
    })
}

/// "6am", "6:30pm", "6 pm", "18:00" → (hour 0-23, minute).
fn clock_time(s: &str) -> Option<(u32, u32)> {
    let s = s.trim_start();
    let h: String = s.chars().take_while(char::is_ascii_digit).collect();
    let mut hour: u32 = h.parse().ok()?;
    let mut rest = &s[h.len()..];
    let mut min = 0;
    if let Some(r) = rest.strip_prefix(':') {
        let m: String = r.chars().take_while(char::is_ascii_digit).collect();
        min = m.parse().ok().filter(|m| *m < 60)?;
        rest = &r[m.len()..];
    }
    let rest = rest.trim_start().replace('.', "");
    if rest.starts_with("am") || rest.starts_with("pm") {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        if rest.starts_with("pm") {
            hour += 12;
        }
    } else if h.len() > 2 || hour > 23 || !s[h.len()..].starts_with(':') {
        // a bare number isn't a time ("resets in 5")
        return None;
    }
    Some((hour, min))
}

/// "06:00" today, "tomorrow 06:00", else "Oct 2, 06:00".
pub fn say(at: i64, now: i64) -> String {
    let (a, n) = (clock::local(at), clock::local(now));
    let hm = format!("{:02}:{:02}", a.hour, a.min);
    let day = |l: clock::Local| crate::panes::calendar::days_from_civil(l.year as i64, l.month, l.day);
    match day(a) - day(n) {
        0 => hm,
        1 => format!("tomorrow {hm}"),
        _ => format!("{} {}, {hm}", cap(MONTHS[(a.month as usize).saturating_sub(1).min(11)]), a.day),
    }
}

/// "2h 14m", "5m", "40s": how long until `at`.
pub fn left(at: i64, now: i64) -> String {
    let s = (at - now).max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed "now" at a known local time: 10:00:00 today.
    fn ten_am() -> i64 {
        let now = clock::now_secs();
        let l = clock::local(now);
        now - ((l.hour as i64 - 10) * 3600 + l.min as i64 * 60 + l.sec as i64)
    }

    #[test]
    fn chat_limit_reset_times() {
        let now = ten_am();
        let hm = |t: i64| {
            let l = clock::local(t);
            (l.hour, l.min)
        };
        let day = |t: i64| {
            let l = clock::local(t);
            crate::panes::calendar::days_from_civil(l.year as i64, l.month, l.day)
        };
        let d0 = day(now);
        // Claude Code, today and tomorrow
        let t = reset_at("You've hit your limit · resets 6pm (Europe/Berlin)", now).unwrap();
        assert_eq!((hm(t), day(t) - d0), ((18, 0), 0));
        let t = reset_at("You've hit your weekly limit · resets 6am", now).unwrap();
        assert_eq!((hm(t), day(t) - d0), ((6, 0), 1), "6am has passed today: tomorrow's");
        // with a date (next week or so)
        let in3 = clock::local(now + 3 * 86_400);
        let t = reset_at(&format!("You've hit your weekly limit · resets {} {}, 6am", MONTHS[in3.month as usize - 1], in3.day), now).unwrap();
        assert_eq!((hm(t), day(t) - d0), ((6, 0), 3));
        // older CLIs: the epoch after a bar
        assert_eq!(reset_at("Claude AI usage limit reached|1759370400", now), Some(1_759_370_400));
        // Codex
        let t = reset_at("You've hit your usage limit. Upgrade to Pro or try again at 3:05 PM.", now).unwrap();
        assert_eq!(hm(t), (15, 5));
        assert_eq!(reset_at("You've hit your usage limit. Try again in 2 hours 5 minutes.", now), Some(now + 2 * 3600 + 300));
        assert_eq!(reset_at("usage limit: try again in 3 days 1 hour", now), Some(now + 3 * 86_400 + 3600));
        // not a limit, or no time in it
        assert_eq!(reset_at("rate limited, retrying", now), None);
        assert_eq!(reset_at("the build failed at 6pm", now), None);
        assert_eq!(reset_at("limit resets in 5", now), None);
        // how it's said
        assert_eq!(say(now + 3600, now), "11:00");
        assert!(say(now + 86_400, now).starts_with("tomorrow"));
        assert_eq!(left(now + 2 * 3600 + 14 * 60, now), "2h 14m");
    }
}
