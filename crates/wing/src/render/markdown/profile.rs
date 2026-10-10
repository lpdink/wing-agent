//! Rendering profile — which kind of cell text belongs to.
//!
//! Reasoning (`Thinking`) and assistant content (`Content`) run through the same markdown pipeline and
//! produce the same code blocks. Three rules are owned here so the full renderer and the streaming
//! engine cannot drift apart:
//!
//! - **Inline fence normalization** (`text:```rust` → a real fence): only `Content`. Reasoning discusses
//!   code fences in prose, so normalizing would turn every mention of ``` into a spurious code block.
//! - **Indented (4-space) blocks**: `Content` keeps them as code blocks (CommonMark: an indented block
//!   *is* code); `Thinking` renders them as prose, because reasoning uses indentation for nesting.
//! - **Math delimiter normalization** (`\(…\)` → `$…$`, `\[…\]` → `$$…$$`, a bare
//!   `\begin{align}…\end{align}` → `$$…$$`): **the same in both profiles**. It is a source rewrite done
//!   before parsing (see [`super::math`]), which is what makes it safe on a streaming slice as well as on
//!   the whole document; the rule only fires on a *complete*, code-free span — an unterminated `\(` or one
//!   spanning a blank line stays literal text, so it cannot swallow prose either way.
//!
//! Everything else (fenced code blocks, highlighting, gutters) is shared: a code block looks the same in
//! both profiles, only the surrounding prose is recolored by the cell compose (see
//! [`super::thinking_segment_style`]).

/// Nesting budget for indented-as-prose blocks (see
/// [`Profile::indented_blocks_are_prose`]).
///
/// Each nesting level re-parses the block's text with the remaining budget
/// minus one, so a degenerate stream (thousands of indent levels in one
/// block) would otherwise cost O(depth × text) per frame while streaming and
/// recurse one parser deep per level — a stack overflow the TUI cannot catch.
/// Real reasoning nests a handful of levels at most; past the budget the
/// block renders as a code block again, which is what it looks like at that
/// indentation anyway.
pub const PROSE_DEPTH_LIMIT: u8 = 8;

/// Which cell a text slice belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Reasoning — prose-first (see the module docs).
    Thinking,
    /// Assistant content — a plain CommonMark document.
    Content,
}

impl Profile {
    /// Whether inline ``` glued to text is normalized into a line-level
    /// fence before parsing.
    pub fn normalizes_inline_fences(self) -> bool {
        matches!(self, Profile::Content)
    }

    /// Whether indented (4-space) blocks are rendered as prose instead of
    /// code blocks.
    pub fn indented_blocks_are_prose(self) -> bool {
        matches!(self, Profile::Thinking)
    }

    /// Whether math delimiters pulldown does not understand are normalized
    /// before parsing (`\(…\)` → `$…$`, `\[…\]` → `$$…$$`, a bare
    /// `\begin{align}…\end{align}` → `$$…$$`).
    ///
    /// The same in both profiles — see the module docs for why this rule is
    /// still owned by `Profile`. Turning math *off* is a config decision
    /// (`rendering.math`), so the renderer ANDs this with
    /// [`crate::config::rendering::MathMode`].
    pub fn normalizes_math_delimiters(self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule table the docs describe (and the handoff asks for):
    /// two per-profile differences, one shared normalization rule.
    #[test]
    fn rule_table_matches_the_docs() {
        for profile in [Profile::Content, Profile::Thinking] {
            // Rule 3 is shared: it applies to both profiles.
            assert!(profile.normalizes_math_delimiters(), "{profile:?}");
        }
        // Rule 1 and rule 2 are the two differences.
        assert!(Profile::Content.normalizes_inline_fences());
        assert!(!Profile::Thinking.normalizes_inline_fences());
        assert!(Profile::Thinking.indented_blocks_are_prose());
        assert!(!Profile::Content.indented_blocks_are_prose());
    }
}
