//! agents (being built)

use crate::pane::{Cx, Pane};
use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

/// `oriel report ...` — called by agent hooks (see the orchestrator). Returns the process exit code.
pub fn cli(_args: &[String]) -> i32 {
    0
}

pub struct Agents {}

impl Agents {
    pub fn new(_cfg: &crate::config::Config) -> Self {
        Self {}
    }
}

impl Pane for Agents {
    fn title(&self) -> String {
        "agents".into()
    }
    fn icon(&self) -> &'static str {
        "robot"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        super::stub::draw(f, area, cx, "agents");
    }
    fn key(&mut self, _key: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}
