//! `wing stop` — stop gateway daemon.

use std::time::Duration;

use super::state::{WingState, is_gateway_running};

/// Stop the gateway daemon gracefully.
///
/// Sends SIGTERM, waits up to 3 seconds, then SIGKILL if still alive.
pub fn stop_gateway() -> anyhow::Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("wing stop is not supported on this platform");

    #[cfg(unix)]
    {
        let state = WingState::load();
        let gw = match state.gateway {
            Some(ref gw) if is_gateway_running(gw) => gw.clone(),
            _ => {
                println!("Gateway is not running");
                WingState::clear_gateway();
                return Ok(());
            }
        };

        let pid = gw.pid as i32;

        // Send SIGTERM. Check return value — EPERM means wrong user,
        // ESRCH means process already gone.
        let ret = unsafe { libc::kill(pid, libc::SIGTERM) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            WingState::clear_gateway();
            anyhow::bail!("Failed to send SIGTERM to PID {pid}: {err}");
        }

        // Wait up to 3 seconds for graceful shutdown.
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if !is_gateway_running(&gw) {
                break;
            }
        }

        // If still alive, SIGKILL.
        if is_gateway_running(&gw) {
            let ret = unsafe { libc::kill(pid, libc::SIGKILL) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                WingState::clear_gateway();
                anyhow::bail!("Failed to send SIGKILL to PID {pid}: {err}");
            }
            std::thread::sleep(Duration::from_millis(100));
            println!("Gateway forcefully stopped (PID {pid})");
        } else {
            println!("Gateway stopped (PID {pid})");
        }

        WingState::clear_gateway();
        Ok(())
    }
}
