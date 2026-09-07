//! Application heartbeat policy for transports whose platform hides WebSocket Pong.

pub struct Heartbeat {
    deadline_ms: u64,
    awaiting_reply: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    Wait(u64),
    Ping,
    Lost,
}

impl Heartbeat {
    pub fn new(now_ms: u64) -> Self {
        Self {
            deadline_ms: now_ms.saturating_add(15_000),
            awaiting_reply: false,
        }
    }

    pub fn received(&mut self, now_ms: u64) {
        *self = Self::new(now_ms);
    }

    pub fn tick(&mut self, now_ms: u64) -> Tick {
        if now_ms < self.deadline_ms {
            Tick::Wait(self.deadline_ms - now_ms)
        } else if self.awaiting_reply {
            Tick::Lost
        } else {
            self.awaiting_reply = true;
            self.deadline_ms = now_ms.saturating_add(20_000);
            Tick::Ping
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_browser_probes_then_reconnects_and_any_reply_recovers() {
        let mut heartbeat = Heartbeat::new(0);
        assert_eq!(heartbeat.tick(14_999), Tick::Wait(1));
        assert_eq!(heartbeat.tick(15_000), Tick::Ping);
        assert_eq!(heartbeat.tick(34_999), Tick::Wait(1));
        assert_eq!(heartbeat.tick(35_000), Tick::Lost);
        let mut heartbeat = Heartbeat::new(0);
        assert_eq!(heartbeat.tick(15_000), Tick::Ping);
        heartbeat.received(34_999);
        // A timeout queued before an incoming reply must not kill the socket.
        assert_eq!(heartbeat.tick(35_000), Tick::Wait(14_999));
        assert_eq!(heartbeat.tick(49_999), Tick::Ping);
        assert_eq!(heartbeat.tick(69_999), Tick::Lost);
    }
}
