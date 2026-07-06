//! Link handling utilities.
//!
//! Adapted from VTCode (MIT license). Simplified to remove regex dependency.

use super::types::MarkdownSegment;

/// Whether to render the link destination URL after the link text.
///
/// Local file paths are hidden (the link text alone is sufficient),
/// while remote URLs are shown for reference.
pub(crate) fn should_render_link_destination(dest_url: &str) -> bool {
    !is_local_path_like_link(dest_url)
}

/// Check if any of the label segments already have a location suffix.
pub(crate) fn label_segments_have_location_suffix(segments: &[MarkdownSegment]) -> bool {
    let Some(last) = segments.last() else {
        return false;
    };
    if label_has_location_suffix(&last.text) {
        return true;
    }
    if segments.len() == 1 {
        return false;
    }
    let mut label = String::with_capacity(segments.iter().map(|s| s.text.len()).sum());
    for segment in segments {
        label.push_str(&segment.text);
    }
    label_has_location_suffix(&label)
}

/// Extract a hidden location suffix from a local file link destination.
///
/// For links like `[text](./file.rs#L10)`, returns `Some("#L10")`.
pub(crate) fn extract_hidden_location_suffix(dest_url: &str) -> Option<String> {
    if !is_local_path_like_link(dest_url) {
        return None;
    }
    // Look for hash location suffix like #L10, #L10C5.
    if let Some((_, fragment)) = dest_url.rsplit_once('#')
        && is_hash_location_suffix(fragment)
    {
        return Some(format!("#{fragment}"));
    }
    // Look for colon location suffix like :10, :10:5.
    if let Some(suffix) = extract_colon_suffix(dest_url) {
        return Some(suffix);
    }
    None
}

fn label_has_location_suffix(text: &str) -> bool {
    // Check for hash suffix like #L10.
    if let Some((_, fragment)) = text.rsplit_once('#')
        && is_hash_location_suffix(fragment)
    {
        return true;
    }
    // Check for colon suffix like :10 or :10:5.
    extract_colon_suffix(text).is_some()
}

/// Check if fragment matches `L\d+(C\d+)?` pattern.
fn is_hash_location_suffix(fragment: &str) -> bool {
    let s = fragment;
    if !s.starts_with('L') {
        return false;
    }
    let after_l = &s[1..];
    if after_l.is_empty() {
        return false;
    }
    // Parse digits.
    let digits_end = after_l
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_l.len());
    if digits_end == 0 {
        return false;
    }
    let rest = &after_l[digits_end..];
    if rest.is_empty() {
        return true; // L10
    }
    // Optional C\d+ part.
    if !rest.starts_with('C') {
        return false;
    }
    let after_c = &rest[1..];
    !after_c.is_empty() && after_c.chars().all(|c| c.is_ascii_digit())
}

/// Extract trailing `:digits` or `:digits:digits` suffix.
fn extract_colon_suffix(text: &str) -> Option<String> {
    // Find the last colon that starts a number.
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len < 2 {
        return None;
    }

    // Walk backwards to find `:digits` or `:digits:digits`.
    let mut i = len;
    // Find trailing digits.
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == len || i == 0 || bytes[i - 1] != b':' {
        return None;
    }
    let colon_pos = i - 1;
    // Check for optional second `:digits` part.
    if colon_pos > 0 {
        let before = &text[..colon_pos];
        if let Some(second_colon) = before.rfind(':') {
            let between = &before[second_colon + 1..];
            if !between.is_empty() && between.chars().all(|c| c.is_ascii_digit()) {
                return Some(text[second_colon..].to_string());
            }
        }
    }
    Some(text[colon_pos..].to_string())
}

fn is_local_path_like_link(dest_url: &str) -> bool {
    dest_url.starts_with("file://")
        || dest_url.starts_with('/')
        || dest_url.starts_with("~/")
        || dest_url.starts_with("./")
        || dest_url.starts_with("../")
        || dest_url.starts_with("\\\\")
        || matches!(
            dest_url.as_bytes(),
            [drive, b':', separator, ..]
                if drive.is_ascii_alphabetic() && matches!(separator, b'/' | b'\\')
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_url_should_render() {
        assert!(should_render_link_destination("https://example.com"));
        assert!(should_render_link_destination("http://foo.bar"));
    }

    #[test]
    fn local_path_should_not_render() {
        assert!(!should_render_link_destination("./src/main.rs"));
        assert!(!should_render_link_destination("../lib.rs"));
        assert!(!should_render_link_destination("~/config"));
        assert!(!should_render_link_destination("/absolute/path"));
        assert!(!should_render_link_destination("file:///foo"));
    }

    #[test]
    fn extract_hash_location_suffix() {
        assert_eq!(
            extract_hidden_location_suffix("./file.rs#L10"),
            Some("#L10".to_string())
        );
        assert_eq!(
            extract_hidden_location_suffix("./file.rs#L10C5"),
            Some("#L10C5".to_string())
        );
        assert_eq!(extract_hidden_location_suffix("./file.rs"), None);
    }

    #[test]
    fn extract_colon_location_suffix() {
        assert_eq!(
            extract_hidden_location_suffix("./file.rs:10"),
            Some(":10".to_string())
        );
        assert_eq!(
            extract_hidden_location_suffix("./file.rs:10:5"),
            Some(":10:5".to_string())
        );
    }

    #[test]
    fn no_suffix_for_remote_url() {
        assert_eq!(extract_hidden_location_suffix("https://example.com"), None);
    }

    #[test]
    fn label_has_location_suffix_detection() {
        assert!(label_has_location_suffix("main.rs:10"));
        assert!(label_has_location_suffix("main.rs#L10"));
        assert!(!label_has_location_suffix("main.rs"));
    }

    #[test]
    fn hash_location_suffix_validation() {
        assert!(is_hash_location_suffix("L10"));
        assert!(is_hash_location_suffix("L10C5"));
        assert!(!is_hash_location_suffix("L"));
        assert!(!is_hash_location_suffix("LC5"));
        assert!(!is_hash_location_suffix("foo"));
    }
}
