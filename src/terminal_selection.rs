use ratatui::{buffer::Buffer, layout::Position, style::Modifier};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TerminalSelection {
    anchor: Option<Position>,
    focus: Option<Position>,
}

impl TerminalSelection {
    pub(crate) fn start(&mut self, position: Position) {
        self.anchor = Some(position);
        self.focus = Some(position);
    }

    pub(crate) fn extend(&mut self, position: Position) {
        if self.anchor.is_some() {
            self.focus = Some(position);
        }
    }

    pub(crate) fn finish(&mut self, buffer: &Buffer) -> Option<String> {
        let text = self.text(buffer);
        *self = Self::default();
        text
    }

    pub(crate) fn has_range(&self) -> bool {
        matches!((self.anchor, self.focus), (Some(anchor), Some(focus)) if anchor != focus)
    }

    pub(crate) fn text(&self, buffer: &Buffer) -> Option<String> {
        let (start, end) = self.ordered_bounds()?;
        if start == end || !buffer.area.contains(start) || !buffer.area.contains(end) {
            return None;
        }

        let mut lines = Vec::with_capacity((end.y - start.y + 1) as usize);
        for row in start.y..=end.y {
            let first_column = if row == start.y {
                start.x
            } else {
                buffer.area.x
            };
            let last_column = if row == end.y {
                end.x
            } else {
                buffer.area.right().saturating_sub(1)
            };
            let mut line = String::new();
            for column in first_column..=last_column {
                if let Some(cell) = buffer.cell(Position::new(column, row)) {
                    line.push_str(cell.symbol());
                }
            }
            lines.push(line.trim_end().to_owned());
        }
        let text = lines.join("\n");
        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn highlight(&self, buffer: &mut Buffer) {
        let Some((start, end)) = self.ordered_bounds() else {
            return;
        };
        if start == end || !buffer.area.contains(start) || !buffer.area.contains(end) {
            return;
        }
        for row in start.y..=end.y {
            let first_column = if row == start.y {
                start.x
            } else {
                buffer.area.x
            };
            let last_column = if row == end.y {
                end.x
            } else {
                buffer.area.right().saturating_sub(1)
            };
            for column in first_column..=last_column {
                if let Some(cell) = buffer.cell_mut(Position::new(column, row)) {
                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }

    fn ordered_bounds(&self) -> Option<(Position, Position)> {
        let anchor = self.anchor?;
        let focus = self.focus?;
        if (anchor.y, anchor.x) <= (focus.y, focus.x) {
            Some((anchor, focus))
        } else {
            Some((focus, anchor))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalSelection;
    use ratatui::{
        buffer::Buffer,
        layout::{Position, Rect},
    };

    #[test]
    fn a_drag_extracts_rendered_text_without_terminal_padding() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        buffer.set_string(0, 0, "hello", ratatui::style::Style::default());
        buffer.set_string(0, 1, "world", ratatui::style::Style::default());
        let mut selection = TerminalSelection::default();

        selection.start(Position::new(1, 0));
        selection.extend(Position::new(3, 1));

        assert_eq!(selection.text(&buffer).as_deref(), Some("ello\nworl"));
    }

    #[test]
    fn active_selection_is_visibly_highlighted() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        buffer.set_string(0, 0, "hello", ratatui::style::Style::default());
        let mut selection = TerminalSelection::default();
        selection.start(Position::new(1, 0));
        selection.extend(Position::new(3, 0));

        selection.highlight(&mut buffer);

        assert!(
            !buffer
                .cell(Position::new(0, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        for column in 1..=3 {
            assert!(
                buffer
                    .cell(Position::new(column, 0))
                    .unwrap()
                    .style()
                    .add_modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            );
        }
        assert!(
            !buffer
                .cell(Position::new(4, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }

    #[test]
    fn release_returns_dragged_text_and_clears_the_highlight() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        buffer.set_string(0, 0, "hello", ratatui::style::Style::default());
        let mut selection = TerminalSelection::default();
        selection.start(Position::new(1, 0));
        selection.extend(Position::new(3, 0));

        assert_eq!(selection.finish(&buffer).as_deref(), Some("ell"));
        assert_eq!(selection, TerminalSelection::default());
    }
}
