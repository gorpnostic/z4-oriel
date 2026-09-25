//! notes (placeholder until the real app lands)

use crate::pane::{Cx, Pane};
use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

pub struct Notes {}

impl Notes {
    pub fn new() -> Self { Self {} }
}

impl Pane for Notes {
    fn title(&self) -> String {
        "notes".into()
    }
    fn icon(&self) -> &'static str {
        "notes"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        super::stub::draw(f, area, cx, "notes");
    }
    fn key(&mut self, _key: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}