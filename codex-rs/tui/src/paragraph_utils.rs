use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

/// Convenience to build a Paragraph with wrapping disabled trimming (preserving spaces).
pub fn wrapped_paragraph<'a>(lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines).wrap(Wrap { trim: false })
}



