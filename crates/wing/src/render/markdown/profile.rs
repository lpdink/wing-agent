//! Rendering profile — which kind of cell text belongs to.
//!
//! Reasoning (`Thinking`) and assistant content (`Content`) run through the
//! same markdown pipeline and produce the same code blocks; they differ in
//! exactly two rendering rules, both modeled here so the full renderer and
//! the streaming engine cannot drift apart:
//!
//! - **Inline fence normalization** (`text:```rust` → a real fence): only
//!   `Content`. Reasoning discusses code fences in prose, so normalizing
//!   would turn every mention of ``` into a spurious code block.
//! - **Indented (4-space) blocks**: `Content` keeps them as code blocks
//!   (CommonMark: an indented block *is* code); `Thinking` renders them as
//!   prose, because reasoning uses indentation for nesting — models indent
//!   sub-thoughts far more often than they write unfenced code.
//!
//! Everything else (fenced code blocks, highlighting, gutters) is shared: a
//! code block looks the same in both profiles, only the surrounding prose is
//! recolored by the cell compose (see [`super::thinking_segment_style`]).

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
}
