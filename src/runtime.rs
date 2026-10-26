//! Wake the UI for terminal input, worker completion or repository events.
//! A one-slot wake queue coalesces notifications; input has bounded backpressure.
use crossterm::event::{self, Event};
use std::{
    io,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::Duration,
};

pub(crate) struct Events {
    input: Receiver<io::Result<Event>>,
    wake: Receiver<()>,
    sender: SyncSender<()>,
}
impl Events {
    pub(crate) fn start() -> io::Result<Self> {
        let (sender, wake) = mpsc::sync_channel(1);
        let (input_sender, input) = mpsc::sync_channel(64);
        let input_wake = sender.clone();
        // This process hosts one panel or one diff viewer. The input reader
        // exits with that invocation; it never touches or renders terminal state.
        thread::Builder::new()
            .name("git-panel-input".into())
            .spawn(move || loop {
                let event = event::read();
                let failed = event.is_err();
                if input_sender.send(event).is_err() {
                    break;
                }
                let _ = input_wake.try_send(());
                if failed {
                    break;
                }
            })?;
        Ok(Self {
            input,
            wake,
            sender,
        })
    }
    pub(crate) fn waker(&self) -> SyncSender<()> {
        self.sender.clone()
    }
    pub(crate) fn next(&self, timeout: Option<Duration>) -> io::Result<Option<Event>> {
        if let Ok(event) = self.input.try_recv() {
            return event.map(Some);
        }
        match timeout {
            Some(timeout) => {
                let _ = self.wake.recv_timeout(timeout);
            }
            None => {
                let _ = self.wake.recv();
            }
        }
        self.input.try_recv().ok().transpose()
    }
}
