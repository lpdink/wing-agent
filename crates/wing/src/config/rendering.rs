//! Rendering mode configuration.

use serde::Deserialize;
use serde::Serialize;

/// Thinking block rendering strategy — **默认展开还是默认折叠**。
///
/// `Ctrl+O` 全局翻转（所有轮一起、会话内保持）；语义与刷光细节见
/// `docs/dev/tui-rendering.md` 第二节·六。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ThinkingMode {
    /// 默认展开：reasoning 正文完整渲染（旧行为）。
    #[default]
    Visible,
    /// 默认折叠：一行摘要（`⦁ 深度思考中 4s`，进行中持续刷光，结束后定格时长）。
    Hidden,
}

impl ThinkingMode {
    /// 这一帧展开吗：`explicit`（Ctrl+O 的全局覆盖）优先，否则看默认。
    pub fn expanded(self, explicit: Option<bool>) -> bool {
        explicit.unwrap_or(self == Self::Visible)
    }

    /// 这一帧带折叠标题行吗（= 有没有"折叠"这层身份）。
    ///
    /// `hidden` 默认折叠、`visible` 默认展开；一旦按过 Ctrl+O，整条 transcript
    /// 就都有折叠身份 —— 展开时标题保留、收起时只剩标题。
    pub fn labeled(self, explicit: Option<bool>) -> bool {
        self == Self::Hidden || explicit.is_some()
    }
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
    /// `text` (render formulas) or `off` (leave LaTeX source verbatim).
    pub math: MathMode,
    /// Markdown image rendering: `off` | `auto` (see [`ImagesMode`]).
    pub images: ImagesMode,
}

impl Default for RenderingConfig {
    fn default() -> Self {
        Self {
            thinking: ThinkingMode::Visible,
            math: MathMode::Text,
            images: ImagesMode::Auto,
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

    /// Every key must round-trip through its own dump: the config is written
    /// with `Serialize` (`wing tui --dump-config`) and read back with
    /// `Deserialize`, and a value that does not parse is silently replaced by
    /// the default (which would turn a deliberately disabled feature back on).
    #[test]
    fn rendering_modes_round_trip_through_a_dump() {
        for thinking in [ThinkingMode::Visible, ThinkingMode::Hidden] {
            for math in [MathMode::Text, MathMode::Off] {
                for images in [ImagesMode::Off, ImagesMode::Auto] {
                    let config = RenderingConfig {
                        thinking,
                        math,
                        images,
                    };
                    let dumped = serde_yaml::to_string(&config).expect("dump");
                    let parsed: RenderingConfig =
                        serde_yaml::from_str(&dumped).expect("parse back");
                    assert_eq!(parsed.thinking, thinking, "thinking round-trip: {dumped}");
                    assert_eq!(parsed.math, math, "math round-trip: {dumped}");
                    assert_eq!(parsed.images, images, "images round-trip: {dumped}");
                }
            }
        }
        // The dump really spells the variant names (not the lowercase input
        // form) — that is what makes the round-trip test above meaningful.
        let dumped = serde_yaml::to_string(&RenderingConfig {
            thinking: ThinkingMode::Hidden,
            math: MathMode::Off,
            images: ImagesMode::Off,
        })
        .expect("dump");
        assert!(dumped.contains("Hidden"), "{dumped}");
        assert!(dumped.contains("Off"), "{dumped}");
        assert!(dumped.contains("math"), "{dumped}");
        assert!(dumped.contains("images"), "{dumped}");
    }

    #[test]
    fn the_expansion_helpers_resolve_explicit_over_the_default() {
        // 默认 + 无覆盖：跟随配置。
        assert!(ThinkingMode::Visible.expanded(None));
        assert!(!ThinkingMode::Hidden.expanded(None));
        // 覆盖优先于默认（两个方向）。
        assert!(!ThinkingMode::Visible.expanded(Some(false)));
        assert!(ThinkingMode::Hidden.expanded(Some(true)));
        // 折叠身份：`hidden` 天然有；`visible` 要看用户按没按过 Ctrl+O。
        assert!(ThinkingMode::Hidden.labeled(None));
        assert!(ThinkingMode::Hidden.labeled(Some(true)));
        assert!(!ThinkingMode::Visible.labeled(None));
        assert!(ThinkingMode::Visible.labeled(Some(false)));
        assert!(ThinkingMode::Visible.labeled(Some(true)));
    }

    #[test]
    fn an_absent_key_is_auto() {
        let config: RenderingConfig =
            serde_yaml::from_str("thinking: hidden").expect("rendering config");
        assert_eq!(config.images, ImagesMode::Auto);
        assert_eq!(config.thinking, ThinkingMode::Hidden);
    }
}
