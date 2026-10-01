//! Terminal graphics capability detection.
//!
//! The only place in the crate that asks the terminal what it can draw. Detection is
//! **injectable** ([`ImageSupport::from_parts`]) so every test in this module tree runs
//! headless — no TTY, no alternate screen, no stdin.
//!
//! # The two-tier rule
//!
//! There are exactly two outcomes: the terminal can draw pixels ([`ImageProtocol::Kitty`],
//! [`ImageProtocol::Sixel`], [`ImageProtocol::Iterm2`]) or it cannot
//! ([`ImageSupport::disabled`]). Unicode half-blocks — `ratatui-image`'s fallback "protocol" —
//! are a character mosaic, not graphics, and the product decision is to keep the existing
//! text rendering instead of degrading into ASCII art. [`ImageProtocol`] therefore has no
//! `Halfblocks` variant: a terminal that only reports half-blocks is *disabled*, by type.

use std::time::Duration;

use ratatui_image::picker::{Picker, ProtocolType};

/// Suffix of the crate's stdio probe. `ratatui-image`'s own default (2000 ms) is a
/// two-second stall at startup for terminals that never answer; we cap it at half a second
/// and let the caller shorten it further.
pub const DEFAULT_DETECT_TIMEOUT: Duration = Duration::from_millis(500);

/// The pixel size of one character cell — the terminal's font size.
///
/// This is a *terminal query* result and must never feed layout math: the number of rows an
/// image reserves is a pure function of container width, image aspect ratio and a cap
/// (see the step's design doc, D3). It only decides the resolution the image is encoded at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellPixels {
    /// Cell width in pixels.
    pub width: u16,
    /// Cell height in pixels.
    pub height: u16,
}

impl CellPixels {
    /// Build a cell size. Both dimensions must be non-zero to render anything meaningful.
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    /// Whether both dimensions are non-zero.
    pub const fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// A graphics protocol we can actually paint pixels with.
///
/// Deliberately has no `Halfblocks`: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageProtocol {
    /// Kitty graphics protocol (unicode placeholders, stateful transmission).
    Kitty,
    /// DEC sixel.
    Sixel,
    /// iTerm2 inline images (`OSC 1337`).
    Iterm2,
}

impl ImageProtocol {
    /// Short lowercase name, for status lines and logs.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::Sixel => "sixel",
            Self::Iterm2 => "iterm2",
        }
    }
}

/// The terminal's graphics capability, decided once at startup.
///
/// Immutable after construction. `is_enabled()` is exactly `protocol().is_some()` — a
/// supported protocol requires a real cell pixel size, so the two never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSupport {
    protocol: Option<ImageProtocol>,
    cell: Option<CellPixels>,
    tmux: bool,
}

impl ImageSupport {
    /// Query the terminal over stdio for its graphics protocol and font size.
    ///
    /// Never fails: an unsupported terminal, a missing font size, an I/O error or a timeout
    /// all collapse into [`ImageSupport::disabled`]. The query writes escape sequences to
    /// stdout and reads stdin, so it must be called **after** entering the alternate screen
    /// and **before** the event loop starts reading terminal events; detection also runs
    /// `tmux set -p allow-passthrough on` when it finds itself inside tmux (upstream
    /// behaviour, unchanged here).
    ///
    /// `timeout` bounds the wait for the terminal's answer; see
    /// [`DEFAULT_DETECT_TIMEOUT`].
    pub fn detect(timeout: Duration) -> Self {
        let options = ratatui_image::picker::cap_parser::QueryStdioOptions {
            timeout,
            ..Default::default()
        };
        match Picker::from_query_stdio_with_options(options) {
            Ok(picker) => {
                let protocol = match picker.protocol_type() {
                    ProtocolType::Kitty => Some(ImageProtocol::Kitty),
                    ProtocolType::Sixel => Some(ImageProtocol::Sixel),
                    ProtocolType::Iterm2 => Some(ImageProtocol::Iterm2),
                    // Half-blocks is the "no graphics protocol found" fallback: disabled.
                    ProtocolType::Halfblocks => None,
                };
                let font = picker.font_size();
                let cell = CellPixels::new(font.width, font.height);
                match protocol {
                    Some(protocol) if cell.is_valid() => Self {
                        protocol: Some(protocol),
                        cell: Some(cell),
                        tmux: picker.tmux_detected(),
                    },
                    _ => Self::disabled(),
                }
            }
            Err(_) => Self::disabled(),
        }
    }

    /// Build a capability by hand — the test/demo injection point, and the escape hatch for
    /// a caller that knows better than the probe (e.g. a future explicit override).
    pub fn from_parts(protocol: ImageProtocol, cell: CellPixels, tmux: bool) -> Self {
        if !cell.is_valid() {
            return Self::disabled();
        }
        Self {
            protocol: Some(protocol),
            cell: Some(cell),
            tmux,
        }
    }

    /// The "cannot draw" state: also what a caller gets when the user turned images off.
    pub const fn disabled() -> Self {
        Self {
            protocol: None,
            cell: None,
            tmux: false,
        }
    }

    /// Whether images can be drawn. When false, [`crate::ui::image::ImageStore`] enqueues no
    /// work, reads no files and writes nothing to the buffer — the caller keeps its existing
    /// text rendering untouched.
    pub const fn is_enabled(&self) -> bool {
        self.protocol.is_some()
    }

    /// The protocol to encode for, or `None` when disabled.
    pub const fn protocol(&self) -> Option<ImageProtocol> {
        self.protocol
    }

    /// The terminal's cell size in pixels, or `None` when disabled.
    pub const fn cell_pixel_size(&self) -> Option<CellPixels> {
        self.cell
    }

    /// Whether the probe found itself inside tmux (graphics then need passthrough).
    pub const fn is_tmux(&self) -> bool {
        self.tmux
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_reports_every_field() {
        let support = ImageSupport::from_parts(ImageProtocol::Kitty, CellPixels::new(8, 18), true);
        assert!(support.is_enabled());
        assert_eq!(support.protocol(), Some(ImageProtocol::Kitty));
        assert_eq!(support.cell_pixel_size(), Some(CellPixels::new(8, 18)));
        assert!(support.is_tmux());
    }

    #[test]
    fn from_parts_rejects_degenerate_cell_size() {
        // A zero-sized cell would make every image encode to a one-pixel smear; the honest
        // answer is "this terminal cannot do it".
        let support = ImageSupport::from_parts(ImageProtocol::Sixel, CellPixels::new(0, 18), false);
        assert!(!support.is_enabled());
        assert_eq!(support.protocol(), None);
        assert_eq!(support.cell_pixel_size(), None);
    }

    #[test]
    fn disabled_is_closed_on_every_accessor() {
        let support = ImageSupport::disabled();
        assert!(!support.is_enabled());
        assert_eq!(support.protocol(), None);
        assert_eq!(support.cell_pixel_size(), None);
        assert!(!support.is_tmux());
    }

    #[test]
    fn is_enabled_matches_protocol_presence() {
        for protocol in [
            ImageProtocol::Kitty,
            ImageProtocol::Sixel,
            ImageProtocol::Iterm2,
        ] {
            let support = ImageSupport::from_parts(protocol, CellPixels::new(10, 20), false);
            assert_eq!(support.is_enabled(), support.protocol().is_some());
            assert_eq!(support.protocol(), Some(protocol));
        }
        assert_eq!(
            ImageSupport::disabled().is_enabled(),
            ImageSupport::disabled().protocol().is_some()
        );
    }

    #[test]
    fn detect_without_a_terminal_degrades_instead_of_panicking() {
        // The whole point of `detect` never failing: under `cargo test` stdin is not a TTY,
        // so the stdio query cannot answer. A zero timeout keeps the test instantaneous.
        let support = ImageSupport::detect(Duration::from_millis(1));
        assert!(!support.is_enabled());
        assert_eq!(support.protocol(), None);
    }

    #[test]
    fn protocol_names_are_stable() {
        assert_eq!(ImageProtocol::Kitty.name(), "kitty");
        assert_eq!(ImageProtocol::Sixel.name(), "sixel");
        assert_eq!(ImageProtocol::Iterm2.name(), "iterm2");
    }
}
