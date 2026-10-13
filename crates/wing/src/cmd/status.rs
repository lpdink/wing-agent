//! `wing status` — show gateway daemon status via HTTP.

use super::backend_config::read_backend_gateway_config;

/// The commit this binary was built from (`build.rs` injects it; `"unknown"`
/// when the build had no git to ask).
const CLI_COMMIT: &str = env!("WING_COMMIT_HASH");

/// Display gateway daemon status by querying the health endpoint.
pub async fn show_status() {
    let config = read_backend_gateway_config();
    let http_base = format!("http://{}:{}", config.host, config.port);

    let client = match wing_api_client::GatewayClient::new(&http_base, None) {
        Ok(c) => c,
        Err(e) => {
            println!("Gateway is not running (failed to create HTTP client: {e})");
            return;
        }
    };

    match client.health().await {
        Ok(health) if health.service == "wing-gateway" => {
            println!("Gateway is running");
            println!("  Endpoint: ws://{}:{}/ws", config.host, config.port);
            println!("  Version:  {}", health.version);
            println!(
                "  Commit:   {}",
                health.commit.as_deref().unwrap_or("unknown")
            );
            println!("  Uptime:   {}", format_duration(health.uptime));
            if let Some(warning) = commit_skew_warning(health.commit.as_deref(), CLI_COMMIT) {
                eprintln!("{warning}");
            }
        }
        _ => {
            println!("Gateway is not running");
        }
    }
}

/// The one-line warning for a CLI / gateway build mismatch, or `None` when the
/// two are not comparable.
///
/// Silence is the right answer for the three ways a side can be hash-less: an
/// older gateway sends no `commit` at all, a source build without git carries
/// the `"unknown"` sentinel, and an empty value means nothing — none of them
/// says the two builds disagree, and "unknown vs a hash" is not a mismatch.
fn commit_skew_warning(gateway: Option<&str>, cli: &str) -> Option<String> {
    let gateway = build_hash(gateway?)?;
    let cli = build_hash(cli)?;
    (gateway != cli).then(|| {
        format!(
            "Warning: CLI build {cli} differs from gateway build {gateway} \
             (one of them is stale)"
        )
    })
}

/// A commit as a comparable build hash: `None` for the no-git sentinel and for
/// an empty value.
fn build_hash(commit: &str) -> Option<&str> {
    (!commit.is_empty() && commit != "unknown").then_some(commit)
}

/// Format seconds into human-readable duration.
fn format_duration(secs: i64) -> String {
    if secs < 0 {
        return "0s".to_string();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Same build on both sides: the report stands on its own.
    #[test]
    fn a_matching_build_is_silent() {
        assert_eq!(commit_skew_warning(Some("a1b2c3d"), "a1b2c3d"), None);
    }

    /// The skew: one line naming both halves, so the reader can tell which
    /// side to rebuild.
    #[test]
    fn a_differing_build_warns_once_on_one_line() {
        let warning = commit_skew_warning(Some("c41e3a1"), "a1b2c3d").expect("a mismatch warns");
        assert!(warning.contains("a1b2c3d"), "{warning}");
        assert!(warning.contains("c41e3a1"), "{warning}");
        assert!(warning.contains("stale"), "{warning}");
        assert!(
            !warning.contains('\n'),
            "the warning is one line: {warning:?}"
        );
    }

    /// A gateway that predates commit injection reports nothing to compare —
    /// "unknown vs hash" is not a mismatch.
    #[test]
    fn a_gateway_without_a_hash_is_silent() {
        assert_eq!(commit_skew_warning(None, "a1b2c3d"), None);
        assert_eq!(commit_skew_warning(Some(""), "a1b2c3d"), None);
        assert_eq!(commit_skew_warning(Some("unknown"), "a1b2c3d"), None);
    }

    /// A CLI built without git reports the sentinel; it cannot claim the
    /// gateway is wrong.
    #[test]
    fn a_cli_without_a_hash_is_silent() {
        assert_eq!(commit_skew_warning(Some("c41e3a1"), "unknown"), None);
        assert_eq!(commit_skew_warning(Some("c41e3a1"), ""), None);
        assert_eq!(commit_skew_warning(None, "unknown"), None);
    }
}
