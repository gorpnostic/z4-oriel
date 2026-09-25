//! your AIs (being built)

use crate::pane::{Cx, Pane};
use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

/// `oriel usage-sink` — Claude Code's statusLine command (reads its JSON on stdin). Returns the exit code.
pub fn cli(_args: &[String]) -> i32 {
    0
}

pub struct Ais {}

impl Ais {
    pub fn new(_cfg: &crate::config::Config) -> Self {
        Self {}
    }
}

impl Pane for Ais {
    fn title(&self) -> String {
        "your AIs".into()
    }
    fn icon(&self) -> &'static str {
        "gauge"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        super::stub::draw(f, area, cx, "your AIs");
    }
    fn key(&mut self, _key: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}
