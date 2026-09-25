//! Events, two ways out.
//!
//! In the **service** every event lands in a bounded, numbered log that
//! `GET /v1/events?since=<seq>` long-polls (`wait_since`). The window and
//! any other client read it; nothing is lost between polls because a client
//! asks for "everything after the last seq I saw" and the log keeps the last
//! `CAPACITY` events.
//!
//! In the **window** the same `emit` forwards to the webview through the
//! emitter `lib.rs` installs, so the frontend's `listen` names are the same
//! whichever process produced the event.

use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
#[cfg(feature = "service")]
use std::time::Duration;

pub const CAPACITY: usize = 512;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: u64,
    pub name: String,
    pub payload: Value,
}

type Emitter = Box<dyn Fn(&str, Value) + Send + Sync>;

static EMITTER: OnceLock<Emitter> = OnceLock::new();

struct Log {
    next_seq: u64,
    events: VecDeque<Event>,
}

fn log() -> &'static Mutex<Log> {
    static L: OnceLock<Mutex<Log>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Log { next_seq: 1, events: VecDeque::with_capacity(CAPACITY) }))
}

#[cfg(feature = "service")]
fn notify() -> &'static tokio::sync::Notify {
    static N: OnceLock<tokio::sync::Notify> = OnceLock::new();
    N.get_or_init(tokio::sync::Notify::new)
}

pub fn set_emitter(f: Emitter) {
    let _ = EMITTER.set(f);
}

pub fn emit(name: &str, payload: Value) {
    {
        let mut l = log().lock().unwrap();
        let seq = l.next_seq;
        l.next_seq += 1;
        if l.events.len() == CAPACITY {
            l.events.pop_front();
        }
        l.events.push_back(Event { seq, name: name.to_string(), payload: payload.clone() });
    }
    #[cfg(feature = "service")]
    notify().notify_waiters();
    if let Some(f) = EMITTER.get() {
        f(name, payload);
    }
}

pub fn tool_changed(name: &str) {
    emit("tool-status-changed", serde_json::json!({ "name": name }));
}

/// The newest sequence number handed out (0 when nothing was emitted yet).
pub fn latest_seq() -> u64 {
    log().lock().unwrap().next_seq - 1
}

/// Events after `since`, oldest first. Empty when nothing new.
pub fn since(since: u64) -> Vec<Event> {
    log().lock().unwrap().events.iter().filter(|e| e.seq > since).cloned().collect()
}

/// Long-poll: return as soon as there is anything after `since`, or an empty
/// list when `timeout` passes. Registers for the notification *before*
/// checking the log so an emit between the two is never missed.
#[cfg(feature = "service")]
pub async fn wait_since(since_seq: u64, timeout: Duration) -> Vec<Event> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let notified = notify().notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        let found = since(since_seq);
        if !found.is_empty() {
            return found;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return Vec::new();
        }
    }
}

#[cfg(all(test, feature = "service"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_is_numbered_bounded_and_wakes_pollers() {
        // The log is process-global and other tests emit too, so every
        // assertion filters for this test's own event names.
        let start = latest_seq();
        emit("evt-a", serde_json::json!(1));
        emit("evt-b", serde_json::json!(2));
        let got: Vec<Event> = since(start).into_iter().filter(|e| e.name.starts_with("evt-")).collect();
        assert_eq!(got.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["evt-a", "evt-b"]);
        assert!(got[1].seq > got[0].seq);

        let after = latest_seq();
        let waiter = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let mut since_seq = after;
            loop {
                let batch = wait_since(since_seq, deadline - tokio::time::Instant::now()).await;
                if batch.is_empty() || batch.iter().any(|e| e.name == "evt-c") {
                    return batch;
                }
                since_seq = batch.last().unwrap().seq;
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        emit("evt-c", serde_json::json!(3));
        let woke = waiter.await.unwrap();
        assert!(woke.iter().any(|e| e.name == "evt-c"), "a poller wakes on emit: {woke:?}");

        for i in 0..(CAPACITY + 10) {
            emit("flood", serde_json::json!(i));
        }
        assert_eq!(log().lock().unwrap().events.len(), CAPACITY, "the log is bounded");
    }
}
