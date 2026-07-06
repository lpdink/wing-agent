//! Helper functions for input area text manipulation.

use unicode_width::UnicodeWidthChar;

/// Maximum number of lines the input area supports.
pub const MAX_INPUT_LINES: usize = 10;

/// Width of the line prefix (`"> "` or `"  "`).
pub const PREFIX_WIDTH: u16 = 2;

/// Prefix for paste placeholder lines.
pub const PASTE_PLACEHOLDER_PREFIX: &str = "[Pasted text #";

/// Check if a line is a paste placeholder.
pub fn is_placeholder_line(line: &str) -> bool {
    line.starts_with(PASTE_PLACEHOLDER_PREFIX)
}

/// Convert a char-based column index to a byte offset within `line`.
pub fn char_to_byte(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

/// Convert a byte offset to a char-based column index within `line`.
#[cfg(test)]
pub fn byte_to_char(line: &str, byte: usize) -> usize {
    line[..byte.min(line.len())].chars().count()
}

/// Truncate `s` to fit within `max_width` display columns.
pub fn truncate_by_width(s: &str, max_width: usize) -> &str {
    let mut width = 0;
    for (i, ch) in s.char_indices() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > max_width {
            return &s[..i];
        }
        width += cw;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_to_byte_ascii() {
        assert_eq!(char_to_byte("hello", 0), 0);
        assert_eq!(char_to_byte("hello", 3), 3);
        assert_eq!(char_to_byte("hello", 5), 5);
    }

    #[test]
    fn char_to_byte_utf8() {
        assert_eq!(char_to_byte("你好", 0), 0);
        assert_eq!(char_to_byte("你好", 1), 3);
        assert_eq!(char_to_byte("你好", 2), 6);
    }

    #[test]
    fn byte_to_char_ascii() {
        assert_eq!(byte_to_char("hello", 0), 0);
        assert_eq!(byte_to_char("hello", 3), 3);
    }

    #[test]
    fn byte_to_char_utf8() {
        assert_eq!(byte_to_char("你好", 0), 0);
        assert_eq!(byte_to_char("你好", 3), 1);
        assert_eq!(byte_to_char("你好", 6), 2);
    }

    #[test]
    fn is_placeholder_line_detects() {
        assert!(is_placeholder_line("[Pasted text #1 +5 lines]"));
        assert!(!is_placeholder_line("hello"));
    }
}
