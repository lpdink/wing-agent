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

/// Math (LaTeX) rendering mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum MathMode {
    /// Render formulas as unicode character grids (`$…$`, `$$…$$`, bare AMS
    /// environments). The default.
    #[default]
    Text,
    /// Do not parse formulas at all: `$…$` stays the literal source text it
    /// is today (no math events, no delimiter normalization).
    Off,
}

impl<'de> Deserialize<'de> for MathMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Case-insensitive: `--dump-config` writes the variant name (`math:
        // Text`), and a config pasted back from it must round-trip.
        match s.to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "off" => Ok(Self::Off),
            other => {
                tracing::warn!("invalid math mode '{other}', falling back to 'text'");
                Ok(Self::Text)
            }
        }
    }
}

/// Rendering configuration section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderingConfig {
    pub thinking: ThinkingMode,
    /// `text` (render formulas) or `off` (leave LaTeX source verbatim).
    pub math: MathMode,
}

impl Default for RenderingConfig {
    fn default() -> Self {
        Self {
            thinking: ThinkingMode::Visible,
            math: MathMode::Text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_mode_parses_both_values_and_falls_back() {
        let cfg: RenderingConfig = serde_yaml::from_str("math: off\n").unwrap();
        assert_eq!(cfg.math, MathMode::Off);
        let cfg: RenderingConfig = serde_yaml::from_str("math: text\n").unwrap();
        assert_eq!(cfg.math, MathMode::Text);
        // `--dump-config` writes the variant name; it must round-trip.
        let cfg: RenderingConfig = serde_yaml::from_str("math: Off\n").unwrap();
        assert_eq!(cfg.math, MathMode::Off);
        // Unknown value warns and keeps the default (never fails the parse).
        let cfg: RenderingConfig = serde_yaml::from_str("math: katex\n").unwrap();
        assert_eq!(cfg.math, MathMode::Text);
        // Absent key keeps the default.
        let cfg: RenderingConfig = serde_yaml::from_str("thinking: hidden\n").unwrap();
        assert_eq!(cfg.math, MathMode::Text);
        assert_eq!(cfg.thinking, ThinkingMode::Hidden);
    }

    #[test]
    fn default_rendering_config_has_math_on() {
        assert_eq!(RenderingConfig::default().math, MathMode::Text);
    }
}
