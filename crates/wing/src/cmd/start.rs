//! `wing start` — spawn gateway daemon.

use std::fs::File;
use std::process::Stdio;
use std::time::Duration;

use super::backend_config::tui_home;
use super::discover::find_gateway_executable;

/// Start the gateway daemon.
///
/// First checks if a gateway is already running via HTTP health check.
/// If not, spawns a new gateway process and polls health until ready.
pub async fn start_gateway(host: &str, port: u16) -> anyhow::Result<()> {
    let http_base = format!("http://{host}:{port}");

    // Check if gateway is already running via health check.
    if let Ok(client) = wing_api_client::GatewayClient::new(&http_base)
        && let Ok(health) = client.health().await
        && health.service == "wing-gateway"
    {
        println!(
            "Gateway already running (ws://{host}:{port}/ws, v{})",
            health.version
        );
        return Ok(());
    }

    // Port might be in use by a non-gateway service.
    // Use a short timeout: on WSL2/container environments, SYN to a closed
    // port may be silently dropped (no RST), causing connect to block for
    // the kernel's full TCP retry window (~63s).
    let port_in_use = matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(format!("{host}:{port}")),
        )
        .await,
        Ok(Ok(_))
    );
    if port_in_use {
        anyhow::bail!(
            "Port {port} is already in use by another process (not wing-gateway). \
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
    let mut child = {
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
    let mut child = std::process::Command::new(&gateway_bin)
        .args(["-H", host, "-p", &port.to_string()])
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn gateway: {e}"))?;

    // Child is intentionally not awaited — it keeps running in the background.
    // `Child::drop` does not kill the process.
    let pid = child.id();

    // Poll health endpoint until gateway is ready (up to 5 seconds).
    let poll_interval = Duration::from_millis(100);
    let timeout = Duration::from_secs(5);
    let deadline = std::time::Instant::now() + timeout;

    let client = wing_api_client::GatewayClient::new(&http_base)
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;

    let mut ready = false;
    loop {
        if std::time::Instant::now() >= deadline {
            break;
        }

        // Check if health endpoint is responding.
        if let Ok(health) = client.health().await
            && health.service == "wing-gateway"
        {
            ready = true;
            break;
        }

        // Check if the child process exited.
        if let Ok(Some(status)) = child.try_wait() {
            let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
            let last_lines: String = log_content
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!("Gateway exited immediately ({status}):\n{last_lines}");
        }

        tokio::time::sleep(poll_interval).await;
    }

    if ready {
        println!("Gateway started (PID {pid}, ws://{host}:{port}/ws)");
    } else {
        eprintln!(
            "⚠ Gateway process spawned (PID {pid}) but not yet reachable after 5s. \
             Check logs: {}",
            log_path.display()
        );
    }
    println!("Log: {}", log_path.display());
    Ok(())
}
