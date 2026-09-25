//! Placeholder body shared by apps that aren't built yet.

use crate::pane::Cx;
use crate::ui;
use ratatui::{Frame, layout::Rect, text::Line, widgets::Paragraph};

pub fn draw(f: &mut Frame, area: Rect, cx: &Cx, what: &str) {
    let p = Paragraph::new(vec![Line::raw(""), Line::styled(format!("{what} is being built"), ui::muted(cx.theme))]).centered();
    f.render_widget(p, area);
}
