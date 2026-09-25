//! ai chat (placeholder until the real app lands)

use crate::pane::{Cx, Pane};
use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

pub struct Chat {}

impl Chat {
    pub fn new(_cfg: &crate::config::Config) -> Self { Self {} }
}

impl Pane for Chat {
    fn title(&self) -> String {
        "ai chat".into()
    }
    fn icon(&self) -> &'static str {
        "ai"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        super::stub::draw(f, area, cx, "ai chat");
    }
    fn key(&mut self, _key: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}