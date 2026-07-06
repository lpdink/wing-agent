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

/// Rendering configuration section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderingConfig {
    pub thinking: ThinkingMode,
}

impl Default for RenderingConfig {
    fn default() -> Self {
        Self {
            thinking: ThinkingMode::Visible,
        }
    }
}
