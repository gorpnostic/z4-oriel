//! storage (placeholder until the real app lands)

use crate::pane::{Cx, Pane};
use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

pub struct Storage {}

impl Storage {
    pub fn new() -> Self { Self {} }
}

impl Pane for Storage {
    fn title(&self) -> String {
        "storage".into()
    }
    fn icon(&self) -> &'static str {
        "storage"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        super::stub::draw(f, area, cx, "storage");
    }
    fn key(&mut self, _key: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}