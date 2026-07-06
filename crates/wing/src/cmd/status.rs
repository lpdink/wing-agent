//! `wing status` — show gateway daemon status.

use super::state::{WingState, is_gateway_running};

/// Display gateway daemon status.
pub fn show_status() {
    let state = WingState::load();
    match state.gateway {
        Some(ref gw) if is_gateway_running(gw) => {
            let uptime = chrono::Utc::now()
                .signed_duration_since(gw.started_at)
                .num_seconds();
            let uptime_str = format_duration(uptime);

            println!("Gateway is running");
            println!("  Endpoint: ws://{}:{}/ws", gw.host, gw.port);
            println!("  PID:      {}", gw.pid);
            println!("  Uptime:   {uptime_str}");
        }
        Some(ref gw) => {
            println!("Gateway is not running (stale state: PID {})", gw.pid);
            WingState::clear_gateway();
        }
        None => {
            println!("Gateway is not running");
        }
    }
}

/// Format seconds into human-readable duration.
fn format_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}
