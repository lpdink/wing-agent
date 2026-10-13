//! `wing status` — show gateway daemon status via HTTP.

use super::backend_config::read_backend_gateway_config;

/// The commit this binary was built from (`build.rs` injects it).
const CLI_COMMIT: &str = env!("WING_COMMIT_HASH");

/// The word for "no build hash", named once because both the report line and
/// the comparison read it: a gateway that never got one prints it, and a source
/// build without git carries it as its own commit. Either way it is the absence
/// of a build to compare against, not a build.
const NO_HASH: &str = "unknown";

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
                health.commit.as_deref().unwrap_or(NO_HASH)
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
/// Silence is the right answer for every way a side can be hash-less: an older
/// gateway sends no `commit` at all, a source build without git carries
/// [`NO_HASH`], and an empty value means nothing. None of them says the two
/// builds disagree — "no hash vs a hash" is not a mismatch.
fn commit_skew_warning(gateway: Option<&str>, cli: &str) -> Option<String> {
    let gateway = build_hash(gateway?)?;
    let cli = build_hash(cli)?;
    (gateway != cli)
        .then(|| format!("Warning: CLI build {cli} differs from gateway build {gateway}"))
}

/// A commit as a comparable build hash: `None` for [`NO_HASH`] and for an empty
/// value.
///
/// Python writes a missing gateway commit as `null` (`health.commit` is an
/// option), so the sentinel is only ever the CLI's own — it is filtered here
/// for the gateway too because the report above already collapses the two into
/// the same word, and a comparison that read them differently would be the
/// surprise.
fn build_hash(commit: &str) -> Option<&str> {
    (!commit.is_empty() && commit != NO_HASH).then_some(commit)
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
    /// side to rebuild. No verdict on which one is stale — the two hashes are
    /// the whole evidence.
    #[test]
    fn a_differing_build_warns_once_on_one_line() {
        let warning = commit_skew_warning(Some("c41e3a1"), "a1b2c3d").expect("a mismatch warns");
        assert!(warning.starts_with("Warning:"), "{warning}");
        assert!(warning.contains("a1b2c3d"), "{warning}");
        assert!(warning.contains("c41e3a1"), "{warning}");
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
