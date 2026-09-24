//! The link to the core: a Unix domain socket carrying newline-delimited JSON.
//! A background thread reads lines into an async channel the GTK main loop
//! drains; writes go straight out under a mutex (messages are tiny).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Bus {
    w: Arc<Mutex<UnixStream>>,
}

impl Bus {
    /// Connect, send the `hello` handshake, and start the reader thread.
    pub fn connect(path: &str, key: &str) -> std::io::Result<(Bus, async_channel::Receiver<String>)> {
        let stream = UnixStream::connect(path)?;
        let reader = stream.try_clone()?;
        let bus = Bus { w: Arc::new(Mutex::new(stream)) };
        bus.send(json!({"cmd": "hello", "key": key}));

        let (tx, rx) = async_channel::unbounded::<String>();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(reader).lines();
            while let Some(Ok(line)) = lines.next() {
                if tx.send_blocking(line).is_err() {
                    break;
                }
            }
            // EOF: closing the sender lets the UI loop notice and quit.
            drop(tx);
        });
        Ok((bus, rx))
    }

    /// Send one JSON message (a newline is appended).
    pub fn send(&self, msg: Value) {
        if let Ok(mut w) = self.w.lock() {
            let _ = w.write_all(msg.to_string().as_bytes());
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }
    }

    /// Convenience for a bare `{"cmd": ...}` plus extra fields.
    pub fn cmd(&self, cmd: &str, extra: Value) {
        let mut m = json!({ "cmd": cmd });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                m[k] = v.clone();
            }
        }
        self.send(m);
    }
}
