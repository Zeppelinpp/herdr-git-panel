//! Target selection and asynchronous delivery of one immutable diff selection.
use crate::{
    host::{self, AgentPane},
    theme::NAVIGATION,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use std::{
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

enum Update {
    Targets(Vec<AgentPane>),
    Sent(String),
}

pub(crate) enum Progress {
    Idle,
    Changed,
    Sent(String),
}

pub(crate) struct Delivery {
    payload: String,
    targets: Vec<AgentPane>,
    selected: usize,
    list: ListState,
    loading: bool,
    sending: bool,
    error: String,
    receiver: Option<Receiver<Result<Update>>>,
    wake: Option<SyncSender<()>>,
    hits: Vec<(Rect, usize)>,
    send_hit: Rect,
    can_send: bool,
    retry_required: bool,
}

impl Delivery {
    pub(crate) fn hide(&mut self) {
        self.can_send = false;
        self.hits.clear();
        self.send_hit = Rect::default();
    }

    pub(crate) fn new(payload: String, wake: Option<SyncSender<()>>) -> Self {
        let mut delivery = Self {
            payload,
            targets: Vec::new(),
            selected: 0,
            list: ListState::default(),
            loading: false,
            sending: false,
            error: String::new(),
            receiver: None,
            wake,
            hits: Vec::new(),
            send_hit: Rect::default(),
            can_send: false,
            retry_required: false,
        };
        delivery.discover();
        delivery
    }

    fn spawn(&mut self, job: impl FnOnce() -> Result<Update> + Send + 'static) {
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        let wake = self.wake.clone();
        thread::spawn(move || {
            let _ = sender.send(job());
            if let Some(wake) = wake {
                let _ = wake.try_send(());
            }
        });
    }

    fn discover(&mut self) {
        if self.receiver.is_some() {
            return;
        }
        self.loading = true;
        self.error.clear();
        self.spawn(|| host::available_agents().map(Update::Targets));
    }

    fn send(&mut self) {
        if self.receiver.is_some() || !self.can_send || self.retry_required {
            return;
        }
        let Some(target) = self.targets.get(self.selected).cloned() else {
            return;
        };
        let text = self.payload.clone();
        self.sending = true;
        self.error.clear();
        self.spawn(move || {
            host::send_selection(&target, &text)?;
            Ok(Update::Sent(format!(
                "{} ({})",
                target.name, target.pane_id
            )))
        });
    }

    pub(crate) fn poll(&mut self) -> Progress {
        let Some(receiver) = &self.receiver else {
            return Progress::Idle;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return Progress::Idle,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("Herdr worker stopped")),
        };
        let was_sending = self.sending;
        self.receiver = None;
        self.loading = false;
        self.sending = false;
        match result {
            Ok(Update::Targets(targets)) => {
                self.retry_required = false;
                self.targets = targets;
                self.selected = self.selected.min(self.targets.len().saturating_sub(1));
            }
            Ok(Update::Sent(label)) => return Progress::Sent(label),
            Err(error) => {
                self.error = format!("{error:#}");
                self.retry_required |= was_sending;
            }
        }
        Progress::Changed
    }

    /// Sending is an external write: a second Enter never queues a duplicate.
    /// Closing the picker is allowed while discovering, before any write starts.
    pub(crate) fn key(&mut self, key: KeyEvent) -> bool {
        if self.sending {
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Char('r') => self.discover(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.targets.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.targets.len().saturating_sub(1),
            KeyCode::Enter => self.send(),
            _ => (),
        }
        false
    }

    pub(crate) fn mouse(&mut self, mouse: MouseEvent) {
        if self.sending {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.send_hit.contains((mouse.column, mouse.row).into()) {
                    self.send();
                } else if let Some((_, row)) = self
                    .hits
                    .iter()
                    .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
                {
                    self.selected = *row;
                }
            }
            MouseEventKind::ScrollDown => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.targets.len().saturating_sub(1))
            }
            MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(1),
            _ => (),
        }
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.hide();
        if area.height < 8 || area.width < 35 {
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new("Enlarge diff area to choose an agent. Esc cancels.")
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let width = area.width.min(90);
        let height = area
            .height
            .min((self.targets.len() as u16).saturating_add(5).clamp(6, 16));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let block = Block::bordered()
            .title("Paste selected diff to agent")
            .border_style(Style::new().fg(Color::Blue));
        let inner = block.inner(popup);
        frame.render_widget(Clear, popup);
        frame.render_widget(block, popup);
        let rows = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);
        self.hits.clear();
        self.send_hit = Rect::default();
        if self.loading {
            frame.render_widget(Paragraph::new("Loading available agents…"), rows[0]);
        } else if self.targets.is_empty() {
            frame.render_widget(
                Paragraph::new("No available agent panes. Press r to refresh.")
                    .wrap(Wrap { trim: false }),
                rows[0],
            );
        } else {
            self.can_send = !self.retry_required;
            let items = self
                .targets
                .iter()
                .map(|target| {
                    let text = format!(
                        "{} · {} · {} · {}",
                        target.pane_id, target.name, target.kind, target.status
                    );
                    ListItem::new(
                        text.chars()
                            .map(|ch| if ch.is_control() { ' ' } else { ch })
                            .collect::<String>(),
                    )
                })
                .collect::<Vec<_>>();
            self.list.select(Some(self.selected));
            frame.render_stateful_widget(
                List::new(items).highlight_style(NAVIGATION),
                rows[0],
                &mut self.list,
            );
            for screen in 0..rows[0].height {
                let index = self.list.offset() + usize::from(screen);
                if index < self.targets.len() {
                    self.hits.push((
                        Rect::new(rows[0].x, rows[0].y + screen, rows[0].width, 1),
                        index,
                    ));
                }
            }
        }
        let message = if self.sending {
            "Pasting selection…"
        } else {
            self.error.as_str()
        };
        frame.render_widget(
            Paragraph::new(message)
                .wrap(Wrap { trim: false })
                .style(Style::new().fg(Color::Yellow)),
            rows[1],
        );
        let hint = if self.retry_required {
            "Inspect target input · r reload before retry · Esc cancel"
        } else {
            "[Send]  Enter paste · r refresh · Esc cancel"
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::new().fg(Color::Blue)),
            rows[2],
        );
        if self.can_send && !self.sending && !self.loading && !self.targets.is_empty() {
            self.send_hit = Rect::new(rows[2].x, rows[2].y, rows[2].width.min(6), rows[2].height);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn picker() -> Delivery {
        Delivery {
            payload: "selected diff".into(),
            targets: (1..=3)
                .map(|i| AgentPane {
                    pane_id: format!("w1:p{i}"),
                    name: format!("Agent {i}"),
                    kind: "codex".into(),
                    status: "idle".into(),
                    workspace_id: "w1".into(),
                    tab_id: "w1:t1".into(),
                    terminal_id: format!("term{i}"),
                    revision: 1,
                })
                .collect(),
            selected: 0,
            list: ListState::default(),
            loading: false,
            sending: false,
            error: String::new(),
            receiver: None,
            wake: None,
            hits: Vec::new(),
            send_hit: Rect::default(),
            can_send: false,
            retry_required: false,
        }
    }

    #[test]
    fn target_navigation_and_mouse_selection_do_not_write_until_send() {
        let mut picker = picker();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| picker.render(f, f.area())).unwrap();
        assert!(picker.can_send);
        picker.key(KeyEvent::from(KeyCode::Down));
        assert_eq!(picker.selected, 1);
        let rect = picker.hits[2].0;
        picker.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(picker.selected, 2);
        assert!(picker.receiver.is_none());
        assert!(picker.key(KeyEvent::from(KeyCode::Esc)));
    }

    #[test]
    fn loading_can_cancel_and_inflight_delivery_never_queues_a_second_write() {
        let mut picker = picker();
        let (sender, receiver) = mpsc::channel();
        picker.receiver = Some(receiver);
        picker.loading = true;
        assert!(picker.key(KeyEvent::from(KeyCode::Esc)));
        picker.loading = false;
        picker.sending = true;
        assert!(!picker.key(KeyEvent::from(KeyCode::Enter)));
        assert!(!picker.key(KeyEvent::from(KeyCode::Esc)));
        sender
            .send(Err(anyhow::anyhow!(
                "Delivery unconfirmed; inspect target input"
            )))
            .ok()
            .unwrap();
        assert!(matches!(picker.poll(), Progress::Changed));
        assert!(!picker.sending);
        assert!(picker.error.contains("unconfirmed"));
        assert_eq!(picker.payload, "selected diff");
        assert!(picker.receiver.is_none());
        assert!(matches!(picker.poll(), Progress::Idle));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| picker.render(f, f.area())).unwrap();
        picker.key(KeyEvent::from(KeyCode::Enter));
        assert!(
            picker.receiver.is_none(),
            "unconfirmed writes require an explicit reload before retry"
        );
    }

    #[test]
    fn an_unreadable_picker_cannot_send_to_an_invisible_target() {
        let mut picker = picker();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| picker.render(f, f.area())).unwrap();
        assert!(picker.can_send);
        picker.hide();
        picker.key(KeyEvent::from(KeyCode::Enter));
        assert!(picker.receiver.is_none());
        terminal.backend_mut().resize(25, 6);
        terminal.draw(|f| picker.render(f, f.area())).unwrap();
        assert!(!picker.can_send);
        picker.key(KeyEvent::from(KeyCode::Enter));
        assert!(picker.receiver.is_none());
    }
}
