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
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
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
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
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

    #[test]
    fn an_absent_key_is_auto() {
        let config: RenderingConfig =
            serde_yaml::from_str("thinking: hidden").expect("rendering config");
        assert_eq!(config.images, ImagesMode::Auto);
        assert_eq!(config.thinking, ThinkingMode::Hidden);
    }
}
