use async_channel::{Receiver, Sender};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tcode_protocol::MAX_LINE_BYTES;

const MAX_QUEUED_BYTES: usize = 2 * MAX_LINE_BYTES;
const MAX_QUEUED_MESSAGES: usize = 1024;

struct Line {
    text: String,
    replace: Option<String>,
}

#[derive(Default)]
struct Pending {
    lines: VecDeque<Line>,
    // Includes the line waiting for the one-slot output channel.
    bytes: usize,
    closed: bool,
}

pub(super) struct Outbox {
    pending: Arc<Mutex<Pending>>,
    wake: Sender<()>,
    input: Receiver<String>,
}

impl Outbox {
    pub(super) fn new(
        input: Receiver<String>,
    ) -> (Self, impl Future<Output = ()> + Send, Receiver<String>) {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (wake, awakened) = async_channel::bounded(1);
        let (output, received) = async_channel::bounded(1);
        let outbox = Self {
            pending: pending.clone(),
            wake,
            input,
        };
        let forward = async move {
            loop {
                let line = {
                    let mut pending = pending.lock().unwrap();
                    if pending.closed {
                        return;
                    }
                    pending.lines.pop_front()
                };
                let Some(line) = line else {
                    if awakened.recv().await.is_err() {
                        return;
                    }
                    continue;
                };
                let bytes = line.text.len();
                if output.send(line.text).await.is_err() {
                    return;
                }
                let mut pending = pending.lock().unwrap();
                if !pending.closed {
                    pending.bytes -= bytes;
                }
            }
        };
        (outbox, forward, received)
    }

    pub(super) fn try_send(&self, text: String, replace: Option<String>) -> Result<(), ()> {
        let mut pending = self.pending.lock().unwrap();
        if pending.closed || self.input.is_closed() {
            return Err(());
        }
        if let Some(key) = &replace {
            // Never move a replacement across a reply or an ordered event.
            let previous = pending
                .lines
                .iter()
                .rposition(|line| line.replace.is_none() || line.replace.as_ref() == Some(key));
            if let Some(index) = previous
                && pending.lines[index].replace.as_ref() == Some(key)
            {
                let old = pending.lines.remove(index).unwrap();
                pending.bytes -= old.text.len();
            }
        }
        if text.len() > MAX_LINE_BYTES
            || pending.bytes + text.len() > MAX_QUEUED_BYTES
            || pending.lines.len() >= MAX_QUEUED_MESSAGES
        {
            drop(pending);
            self.close();
            return Err(());
        }
        pending.bytes += text.len();
        pending.lines.push_back(Line { text, replace });
        drop(pending);
        let _ = self.wake.try_send(());
        Ok(())
    }

    fn close(&self) {
        let mut pending = self.pending.lock().unwrap();
        pending.closed = true;
        pending.lines.clear();
        pending.bytes = 0;
        self.input.close();
        self.wake.close();
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        self.close();
    }
}
