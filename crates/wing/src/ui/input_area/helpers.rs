//! Helper functions for input area text manipulation.

/// Maximum number of lines the input area supports.
pub const MAX_INPUT_LINES: usize = 10;

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
}
