//! Reconnect-flow verification — TUI reconnect after a gateway restart.
//!
//! Drives the *fixed* flow (`app::transport::connect_transport` +
//! `Transport::recover_session`) exactly as the TUI reconnect loop does:
//!
//! 1. `connect_transport` — one WebSocket per gateway episode
//! 2. `recover_session` — resume the session; a 404 (never persisted —
//!    blank TUI whose gateway restarted) silently starts a fresh session
//!    over the same WebSocket
//!
//! Checks:
//! - recovery of a *lost* session converges (Ok, fresh session created)
//! - repeated connect → recover → drop cycles leave the fd count flat
//!   (regression check for the old leak: every failed resume attempt used
//!   to abandon a live WebSocket and leak one fd)
//!
//! Usage (against a sandbox gateway — see the repro from the bug report):
//! ```text
//! # 1. create a session that is never persisted (no user message)
//! # 2. restart the gateway (session now only existed in the dead process)
//! # 3. run:
//! cargo run -p wing --example reconnect_flow_verify -- <ws_url> <http_base> <session_id> [cycles]
//! ```
//!
//! Not part of `cargo test` (examples are only built/run explicitly).

use std::io::Write as _;

use wing::app::transport::GatewayEndpoint;
use wing::app::transport::Transport;
use wing::app::transport::connect_transport;

fn fd_count() -> usize {
    std::fs::read_dir("/dev/fd")
        .map(|d| d.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

fn report(msg: impl AsRef<str>) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", msg.as_ref());
    let _ = out.flush();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let ws_url = args
        .next()
        .unwrap_or_else(|| "ws://127.0.0.1:32599/ws".to_string());
    let http_base = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:32599".to_string());
    let session_id = args
        .next()
        .expect("session_id (lost after gateway restart)");
    let cycles: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);

    let endpoint = GatewayEndpoint {
        ws_url,
        http_base,
        api_key: None,
    };

    let baseline = fd_count();
    report(format!("baseline client fds: {baseline}"));

    // Cycle: gateway episode (connect) → session recovery (404 → fresh) →
    // episode ends (drop). Mirrors the TUI loop across gateway flaps.
    for i in 0..cycles {
        let transport: Transport = connect_transport(&endpoint).await?;
        transport
            .recover_session(&session_id, None)
            .await
            .map_err(|e| anyhow::anyhow!("recovery of lost session failed: {e}"))?;
        // Hold the transport briefly, as the TUI would between flaps.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(transport);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        report(format!(
            "cycle {i}: session recovered (resume, or fresh on 404) · client fds: {} (+{})",
            fd_count(),
            fd_count().saturating_sub(baseline)
        ));
    }

    report(format!(
        "final client fds: {} (baseline {baseline}) — flat means no leak",
        fd_count()
    ));
    Ok(())
}
