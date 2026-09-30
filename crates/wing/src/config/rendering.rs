//! Rendering mode configuration.

use serde::Deserialize;
use serde::Serialize;

/// Thinking block rendering strategy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ThinkingMode {
    /// Full markdown rendering (current default behavior).
    #[default]
    Visible,
    /// Hide content, show only event count indicator.
    Hidden,
}

impl<'de> Deserialize<'de> for ThinkingMode {
    /// Accepts the spelling [`Serialize`] produces as well as the lowercase one.
    ///
    /// `Serialize` writes the variant name (`Visible` / `Hidden`) because the
    /// config is also dumped (`wing tui --dump-config`): a value that does not
    /// parse back would be silently replaced by the default, so "dump, keep,
    /// reload" has to round-trip. Matching is case-insensitive — that is a
    /// strict superset of what was accepted before, so no existing config file
    /// changes meaning.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_lowercase().as_str() {
            "visible" => Ok(Self::Visible),
            "hidden" => Ok(Self::Hidden),
            other => {
                tracing::warn!("invalid thinking mode '{other}', falling back to 'visible'");
                Ok(Self::Visible)
            }
        }
    }
}

/// How markdown images render (the two-tier rule — no third mode).
///
/// The tier is decided once per layout, by the image lane
/// ([`crate::app`]): `auto` probes the terminal at startup and draws real
/// pictures where it can; `off` (or a terminal without a graphics protocol)
/// keeps the existing link rendering, byte for byte.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ImagesMode {
    /// Never draw pictures: markdown images stay on the link path.
    Off,
    /// Draw pictures when the terminal supports a graphics protocol.
    #[default]
    Auto,
}

impl<'de> Deserialize<'de> for ImagesMode {
    /// Case-insensitive, so the value [`Serialize`] writes (`Off` / `Auto`,
    /// as seen in a `--dump-config` dump) parses back to the same variant —
    /// otherwise dumping and reloading would silently turn images back on.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            other => {
                tracing::warn!("invalid images mode '{other}', falling back to 'auto'");
                Ok(Self::Auto)
            }
        }
    }
}

/// Rendering configuration section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderingConfig {
    pub thinking: ThinkingMode,
    /// Markdown image rendering: `off` | `auto` (see [`ImagesMode`]).
    pub images: ImagesMode,
}

impl Default for RenderingConfig {
    fn default() -> Self {
        Self {
            thinking: ThinkingMode::Visible,
            images: ImagesMode::Auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_images_mode_defaults_to_auto() {
        assert_eq!(RenderingConfig::default().images, ImagesMode::Auto);
        assert_eq!(ImagesMode::default(), ImagesMode::Auto);
    }

    #[test]
    fn the_images_mode_parses_both_names_and_falls_back_to_auto() {
        let parse = |value: &str| -> ImagesMode {
            serde_yaml::from_str::<RenderingConfig>(&format!("images: {value}"))
                .expect("rendering config")
                .images
        };
        assert_eq!(parse("off"), ImagesMode::Off);
        assert_eq!(parse("auto"), ImagesMode::Auto);
        // A typo must not disable a feature silently in either direction.
        assert_eq!(parse("OFF!"), ImagesMode::Auto);
    }

    /// Both keys must round-trip through their own dump: the config is written
    /// with `Serialize` (`wing tui --dump-config`) and read back with
    /// `Deserialize`, and a value that does not parse is silently replaced by
    /// the default (which would turn a deliberately disabled feature back on).
    #[test]
    fn rendering_modes_round_trip_through_a_dump() {
        for thinking in [ThinkingMode::Visible, ThinkingMode::Hidden] {
            for images in [ImagesMode::Off, ImagesMode::Auto] {
                let config = RenderingConfig { thinking, images };
                let dumped = serde_yaml::to_string(&config).expect("dump");
                let parsed: RenderingConfig = serde_yaml::from_str(&dumped).expect("parse back");
                assert_eq!(parsed.thinking, thinking, "thinking round-trip: {dumped}");
                assert_eq!(parsed.images, images, "images round-trip: {dumped}");
            }
        }
        // The dump really spells the variant names (not the lowercase input
        // form) — that is what makes the round-trip test above meaningful.
        let dumped = serde_yaml::to_string(&RenderingConfig {
            thinking: ThinkingMode::Hidden,
            images: ImagesMode::Off,
        })
        .expect("dump");
        assert!(dumped.contains("Hidden"), "{dumped}");
        assert!(dumped.contains("Off"), "{dumped}");
    }

    #[test]
    fn an_absent_key_is_auto() {
        let config: RenderingConfig =
            serde_yaml::from_str("thinking: hidden").expect("rendering config");
        assert_eq!(config.images, ImagesMode::Auto);
        assert_eq!(config.thinking, ThinkingMode::Hidden);
    }
}
