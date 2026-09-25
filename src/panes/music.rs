//! music (placeholder until the real app lands)

use crate::pane::{Cx, Pane};
use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

pub struct Music {}

impl Music {
    pub fn new(_cfg: &crate::config::Config) -> Self { Self {} }
}

impl Pane for Music {
    fn title(&self) -> String {
        "music".into()
    }
    fn icon(&self) -> &'static str {
        "music"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        super::stub::draw(f, area, cx, "music");
    }
    fn key(&mut self, _key: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}