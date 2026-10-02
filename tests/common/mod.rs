//! What the stalled-destination tests share: a FIFO to stand in for a log file that stops taking
//! writes, and a GELF listener to see what the other destinations were told.

#![allow(dead_code)]

use std::net::UdpSocket;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Make a FIFO at `path`. Opening it for writing blocks until a reader opens it; once its
/// buffer is full, every write to it blocks until the reader reads.
pub fn mkfifo(path: &Path) {
    let made = std::process::Command::new("mkfifo").arg(path).status();
    assert!(
        made.is_ok_and(|s| s.success()),
        "could not make the FIFO {}",
        path.display()
    );
}

/// A GELF listener on 127.0.0.1 that keeps every record it has read.
pub struct Gelf {
    socket: UdpSocket,
    seen: Vec<Value>,
}

impl Gelf {
    pub fn bind() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind the GELF listener");
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("the GELF listener's read timeout");
        Gelf {
            socket,
            seen: Vec::new(),
        }
    }

    pub fn address(&self) -> String {
        self.socket
            .local_addr()
            .expect("the GELF listener's address")
            .to_string()
    }

    /// The first record, already read or read within `within`, that `pred` accepts.
    pub fn wait_for(&mut self, within: Duration, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        if let Some(hit) = self.seen.iter().find(|v| pred(v)) {
            return Some(hit.clone());
        }
        let deadline = Instant::now() + within;
        let mut buf = vec![0u8; 65_536];
        while Instant::now() < deadline {
            // A read timeout is the loop's tick.
            let Ok((n, _)) = self.socket.recv_from(&mut buf) else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<Value>(&buf[..n]) else {
                continue;
            };
            let hit = pred(&record);
            self.seen.push(record.clone());
            if hit {
                return Some(record);
            }
        }
        None
    }

    /// Every record read so far, for a failure message.
    pub fn seen(&self) -> String {
        self.seen
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A GELF record's string field (`_name` on the wire).
pub fn field<'a>(record: &'a Value, name: &str) -> Option<&'a str> {
    record.get(format!("_{name}"))?.as_str()
}

/// A GELF record's numeric field.
pub fn number(record: &Value, name: &str) -> Option<u64> {
    record.get(format!("_{name}"))?.as_u64()
}

/// Whether a record is of `kind` and about `destination`.
pub fn is(record: &Value, kind: &str, destination: &str) -> bool {
    field(record, "kind") == Some(kind) && field(record, "destination") == Some(destination)
}
