//! `wing tag` — session tags: read, edit, and global inventory.
//!
//! Tags are opaque strings attached to a session's persistent metadata
//! (lowercase recommended; `k=v` is a namespace *convention*, not syntax).
//!
//! ```sh
//! wing tag <sid> scheduler task=wing-tags   # add (positional)
//! wing tag <sid> --remove stale             # remove
//! wing tag <sid>                            # read
//! wing tag --list                           # global inventory + counts
//! ```
//!
//! Add / remove in one call is applied atomically server-side; every
//! operation is idempotent and never hydrates an evicted session.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::collections::BTreeMap;
use std::process::ExitCode;

use anyhow::Result;
use serde::Serialize;
use wing_api_client::models::{SessionInfo, TagSessionResponse};

use super::common;

/// One row of `wing tag --list`.
#[derive(Debug, Serialize, PartialEq)]
pub struct TagListEntry {
    pub tag: String,
    pub count: usize,
}

/// Entry point for `wing tag`.
pub async fn run_tag(
    session_id: Option<&str>,
    tags: &[String],
    remove: &[String],
    list: bool,
    json: bool,
) -> ExitCode {
    match run_tag_inner(session_id, tags, remove, list, json).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wing tag error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_tag_inner(
    session_id: Option<&str>,
    tags: &[String],
    remove: &[String],
    list: bool,
    json: bool,
) -> Result<()> {
    if list {
        if session_id.is_some() || !tags.is_empty() || !remove.is_empty() {
            anyhow::bail!("--list cannot be combined with a session id or tags");
        }
        let (host, port) = common::ensure_gateway().await?;
        let http = common::create_api_client(&host, port)?;
        let resp = http.list_sessions().await?;
        let entries = collect_tag_counts(&resp.sessions);
        if json {
            common::print_json_compact(&entries);
        } else {
            print_tag_list(&entries);
        }
        return Ok(());
    }

    let Some(session_id) = session_id else {
        anyhow::bail!("provide a session id, or --list for the global tag inventory");
    };

    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    // Both None = pure read; both given = one atomic mutation.
    let resp = http
        .tag_session(
            session_id,
            (!tags.is_empty()).then(|| tags.to_vec()),
            (!remove.is_empty()).then(|| remove.to_vec()),
        )
        .await?;

    if json {
        common::print_json_compact(&resp);
    } else {
        print_tag_change(&resp);
    }
    Ok(())
}

/// Aggregate tag counts across sessions (inactive included).
///
/// Sorted by count desc, then tag asc — the most-used tags surface first
/// and the order is total (stable across runs).
fn collect_tag_counts(sessions: &[SessionInfo]) -> Vec<TagListEntry> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for session in sessions {
        for tag in &session.tags {
            *counts.entry(tag.as_str()).or_default() += 1;
        }
    }
    let mut entries: Vec<TagListEntry> = counts
        .into_iter()
        .map(|(tag, count)| TagListEntry {
            tag: tag.to_string(),
            count,
        })
        .collect();
    entries.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
    entries
}

fn print_tag_list(entries: &[TagListEntry]) {
    if entries.is_empty() {
        println!("No tags found.");
        return;
    }
    let width = entries
        .iter()
        .map(|e| e.tag.chars().count())
        .chain(std::iter::once("TAG".len()))
        .max()
        .unwrap_or(3);
    println!("{:<width$} COUNT", "TAG");
    println!("{}", "-".repeat(width + 6));
    for entry in entries {
        println!("{:<width$} {}", entry.tag, entry.count);
    }
}

fn print_tag_change(resp: &TagSessionResponse) {
    for tag in &resp.added {
        println!("+ {tag}");
    }
    for tag in &resp.removed {
        println!("- {tag}");
    }
    if resp.tags.is_empty() {
        println!("(no tags)");
    } else {
        println!("tags: {}", resp.tags.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, tags: &[&str]) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: None,
            created_at: None,
            template_name: None,
            workspace: None,
            last_interaction: None,
            status: "inactive".into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            tag_meta: Default::default(),
            model_id: None,
            model_name: None,
            provider_name: None,
            model_display_name: None,
        }
    }

    #[test]
    fn collect_tag_counts_sorts_by_count_then_name() {
        let sessions = vec![
            session("s1", &["executor", "task=a", "favorite"]),
            session("s2", &["executor", "task=b"]),
            session("s3", &["executor"]),
        ];
        let entries = collect_tag_counts(&sessions);
        assert_eq!(
            entries,
            vec![
                TagListEntry {
                    tag: "executor".into(),
                    count: 3
                },
                TagListEntry {
                    tag: "favorite".into(),
                    count: 1
                },
                TagListEntry {
                    tag: "task=a".into(),
                    count: 1
                },
                TagListEntry {
                    tag: "task=b".into(),
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn collect_tag_counts_empty() {
        assert!(collect_tag_counts(&[]).is_empty());
    }
}
