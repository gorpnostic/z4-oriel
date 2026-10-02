//! Chat's part of the alerts app's "open now" list: the question Claude is waiting on and the approvals it's
//! asking for, each answerable from there. Answers go through the same approve.rs bridge the chat's own keys use.

use super::{Chat, agent, approve, providers};
use crate::alerts::{Kind, Open, Reply};
use crate::pane::Cx;
use crate::ui;

/// The longest command or diff a peek shows.
const BODY: usize = 10;

/// An item's key: what it asks, so an answer meant for one question can't land on the next.
fn qkey(q: &approve::Q) -> String {
    format!("q:{}", q.question)
}

fn akey(a: &approve::Ask) -> String {
    format!("ask:{}:{}", a.label, a.target)
}

impl Chat {
    /// The question on screen (the first one waiting, at the part you're on).
    fn current_q(&self) -> Option<approve::Q> {
        self.questions.front().and_then(|q| q.qs.get(self.qs.idx)).cloned()
    }

    /// What's waiting on you in this chat. Asked before every draw: nothing to build when nothing's pending.
    pub(super) fn open_items(&self) -> Vec<Open> {
        if self.questions.is_empty() && self.asks.is_empty() {
            return vec![];
        }
        let who = providers::label(&self.provider_of()).to_lowercase();
        let mut out = vec![];
        if let Some(cur) = self.current_q() {
            let mut o = Open::new(Kind::Approval, qkey(&cur), format!("{who} asks: {}", cur.question));
            if !cur.header.is_empty() {
                o.detail.push(cur.header.clone());
            }
            o.detail.push(cur.question.clone());
            // the rest of this set, and any sets after it
            let later = self.questions.front().map(|q| q.qs.len() - self.qs.idx - 1).unwrap_or(0) + self.questions.iter().skip(1).map(|q| q.qs.len()).sum::<usize>();
            if later > 0 {
                o.detail.push(format!("({later} more after this one)"));
            }
            o.options = cur.options.iter().map(|(label, about)| if about.is_empty() { label.clone() } else { format!("{label} · {about}") }).collect();
            o.multi = cur.multi;
            if cur.multi {
                o.ticked = if self.qs.ticked.len() == cur.options.len() { self.qs.ticked.clone() } else { vec![false; cur.options.len()] };
            }
            out.push(o);
        }
        for a in &self.asks {
            let mut o = Open::new(Kind::Approval, akey(a), format!("{who} wants to: {} {}", a.label, ui::fit(&a.target, 80)));
            o.detail.push(format!("{} {}", a.label, a.target));
            o.detail.extend(a.body.iter().take(BODY).map(|l| {
                let (kind, _, text) = agent::split_line(l);
                format!("{kind} {text}")
            }));
            if a.body.len() > BODY {
                o.detail.push(format!("… {} more lines", a.body.len() - BODY));
            }
            o.yes_no = true;
            out.push(o);
        }
        out
    }

    /// Answer one from the alerts app. Numbers answer (or tick) the question, y / n the approval.
    pub(super) fn answer_open(&mut self, key: &str, r: Reply, cx: &mut Cx) -> bool {
        if r == Reply::Go {
            // the app has switched here: the question or approval is on screen
            self.scroll = 0;
            return true;
        }
        if let Some(cur) = self.current_q().filter(|q| qkey(q) == key) {
            let n = cur.options.len();
            if self.qs.ticked.len() != n {
                self.qs.ticked = vec![false; n];
            }
            match r {
                Reply::Pick(i) if i < n && cur.multi => {
                    self.qs.sel = i;
                    self.qs.ticked[i] = !self.qs.ticked[i];
                }
                Reply::Pick(i) if i < n => {
                    let a = cur.options[i].0.clone();
                    self.answer_question(&cur.question, a.clone());
                    cx.notify(format!("answered: {a}"));
                }
                Reply::Yes if cur.multi => {
                    let picked: Vec<String> = cur.options.iter().zip(&self.qs.ticked).filter(|(_, t)| **t).map(|(o, _)| o.0.clone()).collect();
                    if picked.is_empty() {
                        return false;
                    }
                    let a = picked.join(", ");
                    self.answer_question(&cur.question, a.clone());
                    cx.notify(format!("answered: {a}"));
                }
                _ => return false,
            }
            return true;
        }
        if let Some(i) = self.asks.iter().position(|a| akey(a) == key) {
            let d = match r {
                Reply::Yes => approve::Decision::Allow,
                Reply::No => approve::Decision::Deny,
                _ => return false,
            };
            if let Some(a) = self.asks.remove(i) {
                let _ = a.reply.send(d);
                cx.notify(format!("{} {} {}", if d == approve::Decision::Allow { "allowed" } else { "denied" }, a.label, ui::fit(&a.target, 50)));
            }
            if i == 0 && !self.asks.is_empty() {
                self.front_changed(); // the next one comes up in the chat: keys typed there get the usual grace
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Stream, store};
    use super::*;
    use crate::testkit::Kit;
    use std::sync::Arc;
    use std::time::Instant;

    fn chat(k: &Kit) -> Chat {
        let mut c = Chat::new(&k.config);
        c.provider = "claude".into();
        c.chat.provider = Some("claude".into());
        c.chat.title = store::title_from("pick a parser");
        c.stream = Some(Stream { stop: Arc::default(), inbox: Arc::default(), status: String::new(), started: Instant::now(), tokens: 0, steer: None, pid: Arc::default(), perms: "edits".into() });
        c
    }

    fn q(question: &str, multi: bool, opts: &[&str]) -> approve::Q {
        approve::Q { question: question.into(), header: String::new(), multi, options: opts.iter().map(|o| (o.to_string(), String::new())).collect() }
    }

    /// Claude's question and approvals show in the open-now list and are answered from it, through the same
    /// reply channels the chat's own keys use; a stale answer (the question moved on) is refused.
    #[test]
    fn chat_open_now_answers_through_the_bridge() {
        let mut k = Kit::new();
        let mut c = chat(&k);
        assert!(c.open_items().is_empty(), "nothing pending, nothing listed");
        let (qtx, qrx) = std::sync::mpsc::channel();
        c.questions.push_back(approve::Question { qs: vec![q("Which parser?", false, &["nom", "winnow"]), q("Which are Copy?", true, &["i32", "String", "bool"])], reply: qtx });
        let (atx, arx) = std::sync::mpsc::channel();
        c.asks.push_back(approve::Ask { label: "Bash".into(), target: "cargo publish".into(), body: vec![agent::line('>', None, "cargo publish")], rule: "Bash(cargo publish:*)".into(), reply: atx });
        let items = c.open_items();
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].text.as_str(), items[0].options.clone()), ("claude code asks: Which parser?", vec!["nom".to_string(), "winnow".to_string()]));
        assert!(items[0].detail.iter().any(|l| l.contains("1 more after this one")), "{:?}", items[0].detail);
        assert!(items[1].yes_no && items[1].text == "claude code wants to: Bash cargo publish" && items[1].detail.iter().any(|l| l == "> cargo publish"));
        // the approval first: n denies it
        let mut cx_actions = vec![];
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &tx, actions: &mut cx_actions, focused: false, time: 0.0 };
        assert!(c.answer_open(&items[1].key, Reply::No, &mut cx));
        assert_eq!(arx.try_recv(), Ok(approve::Decision::Deny));
        assert!(c.asks.is_empty());
        assert!(!c.answer_open(&items[1].key, Reply::Yes, &mut cx), "already answered");
        // 2 answers the question; the next part comes up with its own key
        assert!(c.answer_open(&items[0].key, Reply::Pick(1), &mut cx));
        let next = c.open_items();
        assert_eq!(next[0].text, "claude code asks: Which are Copy?");
        assert!(next[0].multi && next[0].ticked == [false, false, false]);
        assert!(!c.answer_open(&items[0].key, Reply::Pick(0), &mut cx), "an answer for the old question doesn't land on the new one");
        // several can be picked: numbers tick (the chat shows the same ticks), y sends
        assert!(!c.answer_open(&next[0].key, Reply::Yes, &mut cx), "nothing ticked yet");
        assert!(c.answer_open(&next[0].key, Reply::Pick(0), &mut cx));
        assert!(c.answer_open(&next[0].key, Reply::Pick(2), &mut cx));
        assert_eq!(c.open_items()[0].ticked, [true, false, true]);
        let s = k.render(&mut c, 110, 30);
        assert!(s.contains("[x] i32") && s.contains("[x] bool"), "the chat's own picker shows them ticked: {s}");
        let mut cx = Cx { id: 1, theme: &k.theme, config: &k.config, tx: &tx, actions: &mut cx_actions, focused: false, time: 0.0 };
        assert!(c.answer_open(&next[0].key, Reply::Yes, &mut cx));
        let ans = qrx.try_recv().unwrap().unwrap();
        assert_eq!(ans["Which parser?"], "winnow");
        assert_eq!(ans["Which are Copy?"], "i32, bool");
        assert!(c.questions.is_empty() && c.open_items().is_empty());
        assert!(cx_actions.iter().any(|a| matches!(a, crate::pane::Action::Notify(s) if s == "answered: winnow")));
    }
}
