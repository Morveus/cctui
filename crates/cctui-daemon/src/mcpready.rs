//! Readiness of a session's `cctui` MCP relay.
//!
//! Claude Code starts the `mcp-agent` stdio server and the first turn
//! concurrently, so a prompt that calls `CctuiAgent` on turn 1 can race the
//! connection and get "No such tool available". The relay announces itself here
//! the moment it answers `initialize`; the session's `SessionStart` hook blocks
//! on [`wait_until_ready`] until that happens, which holds the first turn.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Poll cadence of [`wait_until_ready`]. The wait is bounded by its caller's
/// timeout, so this only sets how promptly a ready relay releases the turn.
const POLL: Duration = Duration::from_millis(50);

struct State {
    launched: HashMap<String, Instant>,
    ready: HashMap<String, Instant>,
}

static STATE: LazyLock<Mutex<State>> =
    LazyLock::new(|| Mutex::new(State { launched: HashMap::new(), ready: HashMap::new() }));

/// Record that `session_id` is being launched with a relay, so the announce can
/// report how long the connect took.
pub fn note_launch(session_id: &str) {
    if let Ok(mut st) = STATE.lock() {
        st.launched.insert(session_id.to_owned(), Instant::now());
        st.ready.remove(session_id);
    }
}

/// The relay for `session_id` has completed `initialize`.
pub fn announce(session_id: &str) {
    let Ok(mut st) = STATE.lock() else { return };
    let latency_ms = st.launched.get(session_id).map(|t| t.elapsed().as_millis());
    let first = st.ready.insert(session_id.to_owned(), Instant::now()).is_none();
    drop(st);
    if first {
        tracing::info!(session = %session_id, latency_ms = ?latency_ms, "MCP relay connected");
    }
}

/// Whether `session_id`'s relay has announced itself.
#[must_use]
pub fn is_ready(session_id: &str) -> bool {
    STATE.lock().is_ok_and(|st| st.ready.contains_key(session_id))
}

/// Block until `session_id`'s relay is ready, or `timeout` elapses.
///
/// Returns whether it became ready. A timeout is NOT an error: the caller
/// releases the turn anyway, because a relay that never connects must not cost
/// the session its launch.
#[must_use]
pub async fn wait_until_ready(session_id: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if is_ready(session_id) {
            return true;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                session = %session_id,
                timeout_ms = %timeout.as_millis(),
                "MCP relay did not connect before the first turn; releasing it anyway"
            );
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Forget `session_id`, so a relaunch measures its own connect.
pub fn forget(session_id: &str) {
    if let Ok(mut st) = STATE.lock() {
        st.launched.remove(session_id);
        st.ready.remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::{Duration, announce, forget, is_ready, note_launch, wait_until_ready};

    #[tokio::test]
    async fn a_wait_returns_as_soon_as_the_relay_announces() {
        let session = "ready-1";
        forget(session);
        note_launch(session);
        assert!(!is_ready(session));
        let waiter = tokio::spawn(wait_until_ready("ready-1", Duration::from_secs(5)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        announce(session);
        assert!(waiter.await.unwrap(), "the announce must release the wait");
        forget(session);
    }

    /// A relay that never connects releases the turn instead of failing it.
    #[tokio::test]
    async fn a_wait_that_times_out_still_releases_the_turn() {
        let session = "ready-2";
        forget(session);
        note_launch(session);
        assert!(!wait_until_ready(session, Duration::from_millis(80)).await);
        forget(session);
    }

    #[tokio::test]
    async fn an_already_ready_relay_does_not_wait() {
        let session = "ready-3";
        forget(session);
        announce(session);
        assert!(wait_until_ready(session, Duration::from_secs(30)).await);
        forget(session);
    }
}
