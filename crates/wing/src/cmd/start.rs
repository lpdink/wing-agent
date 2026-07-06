//! `wing start` — spawn gateway daemon.

use std::fs::File;
use std::process::Stdio;
use std::time::Duration;

use chrono::Utc;

use super::discover::find_gateway_executable;
use super::state::{GatewayState, WingState, is_gateway_running, tui_home};

/// Start the gateway daemon.
pub fn start_gateway(host: &str, port: u16) -> anyhow::Result<()> {
    // Check if already running.
    let state = WingState::load();
    if let Some(ref gw) = state.gateway
        && is_gateway_running(gw)
    {
        println!(
            "Gateway already running (PID {}, ws://{}:{}/ws)",
            gw.pid, gw.host, gw.port
        );
        return Ok(());
    }

    // Check port availability (something else is already listening).
    if std::net::TcpStream::connect(format!("{host}:{port}")).is_ok() {
        anyhow::bail!(
            "Port {port} is already in use by another process. \
             Use --port to specify a different port."
        );
    }

    // Find gateway executable.
    let gateway_bin = find_gateway_executable()?;

    // Prepare log file (truncate mode).
    let home = tui_home();
    std::fs::create_dir_all(&home)?;
    let log_path = home.join("gateway.log");
    let log_out = File::create(&log_path)?;
    let log_err = log_out.try_clone()?;

    // Spawn gateway process in a new session so it survives parent exit
    // and doesn't receive SIGHUP when the terminal closes.
    #[cfg(unix)]
    let child = {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(&gateway_bin)
            .args(["-H", host, "-p", &port.to_string()])
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err))
            .process_group(0) // setsid equivalent: new process group
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn gateway: {e}"))?
    };

    #[cfg(not(unix))]
    let child = std::process::Command::new(&gateway_bin)
        .args(["-H", host, "-p", &port.to_string()])
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn gateway: {e}"))?;

    // Child is intentionally not awaited — it keeps running in the background.
    // `Child::drop` does not kill the process.
    let pid = child.id();

    // Wait briefly and check if process is alive + port reachable.
    std::thread::sleep(Duration::from_millis(500));

    let gw_state = GatewayState {
        host: host.to_string(),
        port,
        pid,
        started_at: Utc::now(),
    };

    if !is_gateway_running(&gw_state) {
        // Process died quickly — show last log lines.
        let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
        let last_lines: String = log_content
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n");
        WingState::clear_gateway();
        anyhow::bail!("Gateway exited immediately:\n{last_lines}");
    }

    // Check port connectivity — warn if not yet reachable.
    if std::net::TcpStream::connect(format!("{host}:{port}")).is_err() {
        std::thread::sleep(Duration::from_millis(500));
        if std::net::TcpStream::connect(format!("{host}:{port}")).is_err() {
            eprintln!(
                "⚠ Gateway process is alive (PID {pid}) but port {port} is not yet reachable. \
                 Check logs: {}",
                log_path.display()
            );
        }
    }

    // Save state.
    let new_state = WingState {
        gateway: Some(gw_state),
    };
    new_state.save()?;

    println!("Gateway started (PID {pid}, ws://{host}:{port}/ws)");
    println!("Log: {}", log_path.display());
    Ok(())
}
