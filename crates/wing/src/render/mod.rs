//! Rendering utilities.

pub mod ansi;
pub mod diff_highlight;
pub mod fit;
pub mod heatmap;
pub mod markdown;
pub mod picture;
pub mod renderable;
pub mod syntax;
pub mod table;

#[cfg(test)]
pub(crate) mod test_support;
