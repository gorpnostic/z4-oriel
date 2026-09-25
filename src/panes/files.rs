//! files (placeholder until the real app lands)

use crate::pane::{Cx, Pane};
use crossterm::event::KeyEvent;
use ratatui::{Frame, layout::Rect};

pub struct Files {}

impl Files {
    pub fn new(_dir: Option<std::path::PathBuf>) -> Self { Self {} }
}

impl Pane for Files {
    fn title(&self) -> String {
        "files".into()
    }
    fn icon(&self) -> &'static str {
        "files"
    }
    fn render(&mut self, f: &mut Frame, area: Rect, cx: &mut Cx) {
        super::stub::draw(f, area, cx, "files");
    }
    fn key(&mut self, _key: KeyEvent, _cx: &mut Cx) -> bool {
        false
    }
}