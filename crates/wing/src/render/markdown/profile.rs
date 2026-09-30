//! Rendering profile — which kind of cell text belongs to.
//!
//! Reasoning (`Thinking`) and assistant content (`Content`) run through the
//! same markdown pipeline and produce the same code blocks; they differ in
//! exactly one rendering rule today, modeled here so the full renderer and
//! the streaming engine cannot drift apart:
//!
//! - **Inline fence normalization** (`text:```rust` → a real fence): only
//!   `Content`. Reasoning discusses code fences in prose, so normalizing
//!   would turn every mention of ``` into a spurious code block.
//!
//! Everything else (fenced code blocks, highlighting, gutters) is shared: a
//! code block looks the same in both profiles, only the surrounding prose is
//! recolored by the cell compose (see [`super::thinking_segment_style`]).

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
}
