//! Select the visible terminal cells without forwarding a drag to UI controls.
use base64::{engine::general_purpose::STANDARD, Engine};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{buffer::Buffer, layout::Position, style::Style};
use std::io::{self, Write};
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(crate) struct TextSelection {
    screen: Buffer,
    press: Option<MouseEvent>,
    end: Position,
    dragged: bool,
}

pub(crate) enum MouseAction {
    Pending,
    Click(MouseEvent, MouseEvent),
    Copy(String),
    Forward(MouseEvent),
}

impl TextSelection {
    pub(crate) fn active(&self) -> bool {
        self.press.is_some()
    }

    pub(crate) fn cancel(&mut self) {
        self.press = None;
        self.dragged = false;
    }

    pub(crate) fn capture(&mut self, screen: &Buffer) {
        self.screen.clone_from(screen);
    }

    /// While held, render the same snapshot used to extract the copied text.
    pub(crate) fn render(&mut self, buffer: &mut Buffer, style: Style) -> bool {
        if !self.active() {
            return false;
        }
        if buffer.area != self.screen.area {
            self.cancel();
            return false;
        }
        buffer.clone_from(&self.screen);
        if self.dragged {
            self.for_each_cell(|x, y, _, _| {
                buffer[(x, y)].set_style(style);
            });
        }
        true
    }

    pub(crate) fn mouse(&mut self, mouse: MouseEvent) -> MouseAction {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.cancel();
                let position = Position::new(mouse.column, mouse.row);
                if self.screen.area.contains(position) {
                    self.press = Some(mouse);
                    self.end = position;
                }
                MouseAction::Pending
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.extend(mouse);
                MouseAction::Pending
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(press) = self.press else {
                    return MouseAction::Pending;
                };
                self.extend(mouse);
                let action = if self.dragged {
                    MouseAction::Copy(self.text())
                } else {
                    MouseAction::Click(press, mouse)
                };
                self.cancel();
                action
            }
            MouseEventKind::Moved => MouseAction::Pending,
            _ => {
                self.cancel();
                MouseAction::Forward(mouse)
            }
        }
    }

    fn extend(&mut self, mouse: MouseEvent) {
        let Some(press) = self.press else {
            return;
        };
        let area = self.screen.area;
        self.end = Position::new(
            mouse.column.clamp(area.x, area.right() - 1),
            mouse.row.clamp(area.y, area.bottom() - 1),
        );
        self.dragged |= mouse.column != press.column || mouse.row != press.row;
    }

    fn bounds(&self) -> Option<(Position, Position)> {
        let press = self.press?;
        let start = Position::new(press.column, press.row);
        Some(if (start.y, start.x) <= (self.end.y, self.end.x) {
            (start, self.end)
        } else {
            (self.end, start)
        })
    }

    /// Visit whole graphemes, including when an endpoint hits a wide continuation cell.
    fn for_each_cell(&self, mut visit: impl FnMut(u16, u16, &str, bool)) {
        let Some((start, end)) = self.bounds() else {
            return;
        };
        let area = self.screen.area;
        for y in start.y..=end.y {
            let left = if y == start.y { start.x } else { area.x };
            let right = if y == end.y { end.x + 1 } else { area.right() };
            let mut x = area.x;
            while x < area.right() {
                let symbol = self.screen[(x, y)].symbol();
                let width = (symbol.width().max(1) as u16).min(area.right() - x);
                if x < right && x + width > left {
                    for offset in 0..width {
                        visit(x + offset, y, symbol, offset == 0);
                    }
                }
                x += width;
            }
        }
    }

    fn text(&self) -> String {
        let Some((start, end)) = self.bounds() else {
            return String::new();
        };
        let mut lines = vec![String::new(); usize::from(end.y - start.y) + 1];
        self.for_each_cell(|_, y, symbol, first| {
            if first {
                lines[usize::from(y - start.y)].push_str(symbol);
            }
        });
        lines
            .iter()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// OSC 52 reaches the hosting terminal's clipboard, including over SSH.
pub(crate) fn copy(text: &str, mut output: impl Write) -> io::Result<()> {
    // Herdr bounds decoded clipboard writes to 192 KiB. Refuse instead of truncating.
    if text.len() > 192 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Selection exceeds the 192 KiB clipboard limit",
        ));
    }
    if !text.is_empty() {
        write!(output, "\x1b]52;c;{}\x07", STANDARD.encode(text))?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{layout::Rect, style::Color};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn screen(lines: &[&str], width: u16) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
        for (row, line) in lines.iter().enumerate() {
            buffer.set_string(0, row as u16, line, Style::default());
        }
        buffer
    }

    fn copied(action: MouseAction) -> String {
        match action {
            MouseAction::Copy(text) => text,
            _ => panic!("expected a clipboard copy without a click"),
        }
    }

    #[test]
    fn reverse_multiline_drag_preserves_indentation_and_removes_screen_padding() {
        let mut selection = TextSelection::default();
        selection.capture(&screen(&["abcdef", "  中x", "xyz123"], 8));
        selection.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 2));
        selection.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 1, 0));

        let text = copied(selection.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0)));

        assert_eq!(text, "bcdef\n  中x\nxyz");
        assert!(!selection.active());
    }

    #[test]
    fn wide_continuations_and_combining_graphemes_copy_once_in_both_directions() {
        for (start, end) in [(2, 5), (5, 2)] {
            let mut selection = TextSelection::default();
            selection.capture(&screen(&["A中🙂e\u{301}Z"], 10));
            selection.mouse(mouse(MouseEventKind::Down(MouseButton::Left), start, 0));
            selection.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), end, 0));

            let text =
                copied(selection.mouse(mouse(MouseEventKind::Up(MouseButton::Left), end, 0)));

            assert_eq!(text, "中🙂e\u{301}");
        }
    }

    #[test]
    fn rendering_highlight_keeps_the_pressed_snapshot_and_copied_text() {
        let original = screen(&["A中🙂e\u{301}Z"], 10);
        let mut selection = TextSelection::default();
        selection.capture(&original);
        selection.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
        selection.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        let mut refreshed = screen(&["new output"], 10);

        assert!(selection.render(&mut refreshed, Style::default().bg(Color::Blue)));
        for x in 0..10 {
            assert_eq!(refreshed[(x, 0)].symbol(), original[(x, 0)].symbol());
        }
        for x in 1..=4 {
            assert_eq!(refreshed[(x, 0)].bg, Color::Blue);
        }
        assert_ne!(refreshed[(0, 0)].bg, Color::Blue);
        assert_ne!(refreshed[(5, 0)].bg, Color::Blue);
        assert_eq!(
            copied(selection.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 0))),
            "中🙂"
        );
    }

    #[test]
    fn click_is_deferred_until_release_and_drag_never_becomes_a_click() {
        let mut selection = TextSelection::default();
        selection.capture(&screen(&["Commit"], 8));
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 1, 0);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 1, 0);

        assert!(matches!(selection.mouse(down), MouseAction::Pending));
        assert!(selection.active());
        match selection.mouse(up) {
            MouseAction::Click(pressed, released) => {
                assert_eq!(pressed, down);
                assert_eq!(released, up);
            }
            _ => panic!("a stationary release should deliver the deferred click"),
        }
        assert!(!selection.active());

        selection.mouse(down);
        selection.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3, 0));
        selection.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 1, 0));
        assert_eq!(copied(selection.mouse(up)), "o");
        assert!(matches!(selection.mouse(up), MouseAction::Pending));
    }

    #[test]
    fn cancel_and_resize_prevent_stale_release_actions() {
        let mut selection = TextSelection::default();
        selection.capture(&screen(&["old view"], 8));
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 0, 0);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 3, 0);
        selection.mouse(down);
        selection.cancel();
        assert!(!selection.active());
        assert!(matches!(selection.mouse(up), MouseAction::Pending));

        selection.mouse(down);
        selection.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3, 0));
        let mut resized = screen(&["new"], 3);
        let before = resized.clone();
        assert!(!selection.render(&mut resized, Style::default().bg(Color::Blue)));
        assert_eq!(resized, before);
        assert!(!selection.active());
        assert!(matches!(selection.mouse(up), MouseAction::Pending));
    }

    #[test]
    fn wheel_is_forwarded_and_cancels_an_unfinished_press() {
        let mut selection = TextSelection::default();
        selection.capture(&screen(&["text"], 8));
        selection.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        let wheel = mouse(MouseEventKind::ScrollDown, 0, 0);
        match selection.mouse(wheel) {
            MouseAction::Forward(event) => assert_eq!(event, wheel),
            _ => panic!("scrolling must reach the hovered control"),
        }
        assert!(!selection.active());
        assert!(matches!(
            selection.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 0)),
            MouseAction::Pending
        ));
    }

    #[test]
    fn clipboard_output_is_one_bel_terminated_osc52_with_original_utf8() {
        let text = "  中🙂e\u{301}\nnext";
        let mut output = Vec::new();
        copy(text, &mut output).unwrap();
        assert!(output.starts_with(b"\x1b]52;c;"));
        assert_eq!(output.last(), Some(&b'\x07'));
        let decoded = STANDARD.decode(&output[7..output.len() - 1]).unwrap();
        assert_eq!(decoded, text.as_bytes());

        let mut empty = Vec::new();
        copy("", &mut empty).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn clipboard_limit_counts_utf8_bytes_and_refuses_without_partial_output() {
        let at_limit = "中".repeat(64 * 1024);
        assert_eq!(at_limit.len(), 192 * 1024);
        let mut output = Vec::new();
        copy(&at_limit, &mut output).unwrap();
        assert_eq!(
            STANDARD.decode(&output[7..output.len() - 1]).unwrap(),
            at_limit.as_bytes()
        );

        let over_limit = format!("{at_limit}a");
        let mut refused = Vec::new();
        let error = copy(&over_limit, &mut refused).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(refused.is_empty());
    }
}
