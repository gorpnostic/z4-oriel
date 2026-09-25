//! system (placeholder until the real app lands)

use crate::pane::{Cx, Pane};
use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

pub struct System {}

impl System {
    pub fn new() -> Self { Self {} }
}

impl Pane for System {
    fn title(&self) -> String {
        "system".into()
    }
    fn icon(&self) -> &'static str {
        "system"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        super::stub::draw(f, area, cx, "system");
    }
    fn key(&mut self, _key: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}