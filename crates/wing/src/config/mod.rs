//! User configuration — load from `$WING_HOME/tui/config.yaml`.
//!
//! Missing file → silent defaults. Parse error → warn + defaults.

pub mod colors;
pub mod rendering;

use std::path::PathBuf;

use ratatui::style::Color;
use serde::Deserialize;
use serde::Serialize;

use self::colors::parse_color;
use self::rendering::MathMode;
use self::rendering::RenderingConfig;

/// Base palette preset.
///
/// The preset supplies a value for every slot; a slot key set in the config
/// overrides just that slot (see [`ColorsConfig`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ColorPreset {
    /// The designed palette for dark terminals (the default).
    #[default]
    Wing,
    /// Inherit the terminal's own ANSI colours — the pre-preset behaviour,
    /// for light terminals and setups that follow the terminal theme.
    Terminal,
}

impl<'de> Deserialize<'de> for ColorPreset {
    /// Case-insensitive, mirroring the other mode keys: `--dump-config`
    /// writes the variant names (`Wing` / `Terminal`) and must round-trip.
    /// An unknown value warns and keeps the default instead of failing the
    /// whole config parse.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_lowercase().as_str() {
            "wing" => Ok(Self::Wing),
            "terminal" => Ok(Self::Terminal),
            other => {
                tracing::warn!("invalid color preset '{other}', falling back to 'wing'");
                Ok(Self::Wing)
            }
        }
    }
}

impl ColorPreset {
    /// Every slot's base value, before per-slot overrides.
    ///
    /// `Wing` assumes a dark terminal background — a cool neutral ramp
    /// (text → thinking → tool_result → dim) around a cyan accent, with the
    /// status colours pulled toward the brand's navy/amber family. `Terminal`
    /// leaves the named ANSI colours in so the terminal theme decides; the
    /// diff tints stay designed hex in both presets, because a background
    /// tint has to sit under syntax colours and cannot adapt per-theme.
    fn base(self) -> ThemePalette {
        let diff_add_bg = Color::Rgb(0x16, 0x38, 0x1f);
        let diff_del_bg = Color::Rgb(0x47, 0x24, 0x2c);
        let diff_add_bg_strong = Color::Rgb(0x1f, 0x52, 0x30);
        let diff_del_bg_strong = Color::Rgb(0x66, 0x32, 0x3c);
        match self {
            Self::Wing => ThemePalette {
                accent: Color::Rgb(0x3a, 0xc6, 0xe0),
                text: Color::Rgb(0xe6, 0xea, 0xf2),
                thinking: Color::Rgb(0xa5, 0xae, 0xc1),
                tool_result: Color::Rgb(0x87, 0x91, 0xa7),
                dim: Color::Rgb(0x5b, 0x64, 0x78),
                success: Color::Rgb(0x4e, 0xc2, 0x8b),
                warning: Color::Rgb(0xe4, 0xb0, 0x4e),
                danger: Color::Rgb(0xe5, 0x64, 0x6e),
                math: Color::Rgb(0x3a, 0xc6, 0xe0),
                surface: Color::Rgb(0x23, 0x2d, 0x42),
                diff_add_bg,
                diff_del_bg,
                diff_add_bg_strong,
                diff_del_bg_strong,
                math_mode: MathMode::Text,
            },
            Self::Terminal => ThemePalette {
                accent: Color::Cyan,
                text: Color::White,
                thinking: Color::Gray,
                tool_result: Color::DarkGray,
                dim: Color::DarkGray,
                success: Color::Green,
                warning: Color::Yellow,
                danger: Color::Red,
                math: Color::Cyan,
                surface: Color::Rgb(52, 53, 65),
                diff_add_bg,
                diff_del_bg,
                diff_add_bg_strong,
                diff_del_bg_strong,
                math_mode: MathMode::Text,
            },
        }
    }
}

/// Semantic color palette — one slot per UI role.
///
/// [`Self::preset`] picks the base palette; every other key overrides that
/// one slot (absent = follow the preset). `--dump-config` writes only the
/// preset and the overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    /// Base palette (see [`ColorPreset`]).
    pub preset: ColorPreset,
    /// Brand color: logo, commands, links, code, popups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Primary text: messages, model name, input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Thinking: agent reasoning content (more prominent than dim).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Tool result: tool call output (secondary data).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
    /// Quiet register: bullets, borders, gutters, separators — and
    /// tool-call args / timers (secondary metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<String>,
    /// Success: diff+, blockquote, token low.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<String>,
    /// Warning: pending, token mid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Danger: diff-, error, token high.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub danger: Option<String>,
    /// Math: rendered formulas (character grids) and their literal fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub math: Option<String>,
    /// Surface: user message background.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    /// Background tint for diff additions (text keeps syntax colors).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_add_bg: Option<String>,
    /// Background tint for diff deletions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_del_bg: Option<String>,
    /// Background tint for changed words inside an added line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_add_bg_strong: Option<String>,
    /// Background tint for changed words inside a deleted line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_del_bg_strong: Option<String>,
    /// Math rendering mode, resolved from `rendering.math` by
    /// [`AppConfig::resolve`].
    ///
    /// Not a YAML key of this section — the user-facing key is
    /// `rendering.math`. It rides along here because the resolved
    /// [`ThemePalette`] is the only config value the render layer receives
    /// (`render::markdown::full_lines` and `StreamingRender` both take a
    /// palette and nothing else).
    #[serde(skip)]
    pub math_mode: MathMode,
}

/// Layout configuration section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    /// Maximum input area lines.
    pub max_input_lines: usize,
    /// Maximum visible rows in selection popup.
    pub max_popup_rows: usize,
    /// Maximum tool result output lines.
    pub tool_output_max: usize,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            max_input_lines: 10,
            max_popup_rows: 8,
            tool_output_max: 10,
        }
    }
}

/// Top-level application configuration.
///
/// Gateway host:port is NOT stored here — it is read from the backend
/// config (`$WING_HOME/core/config.yaml`) via `backend_config::read_backend_gateway_config()`.
/// This struct only contains TUI-specific settings (colors, layout, rendering).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub colors: ColorsConfig,
    pub layout: LayoutConfig,
    pub rendering: RenderingConfig,
    pub goal: GoalConfig,
    /// API key for gateway authentication.
    /// Empty or None → no auth header sent.
    pub api_key: Option<String>,
}

impl AppConfig {
    /// Fold cross-section settings into the sections the render layer reads.
    ///
    /// Today that is one thing: `rendering.math` → `colors.math_mode`, the
    /// carrier the resolved palette hands to `render::markdown` (see
    /// [`ColorsConfig::math_mode`]). [`AppConfig::load`] calls it; any
    /// programmatic construction that changes `rendering.math` must call it
    /// too, or the renderer keeps the default mode.
    pub fn resolve(&mut self) {
        self.colors.math_mode = self.rendering.math;
    }
}

/// Goal orchestration configuration.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalConfig {
    /// Override the checker agent's system prompt.
    pub checker_system_prompt: Option<String>,
}

impl AppConfig {
    /// Load configuration from `$WING_HOME/tui/config.yaml`.
    ///
    /// Returns defaults if the file doesn't exist or fails to parse.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            tracing::warn!("cannot determine config directory, using defaults");
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_yaml::from_str::<AppConfig>(&content) {
                Ok(mut cfg) => {
                    cfg.resolve();
                    tracing::info!(?path, "loaded config");
                    cfg
                }
                Err(e) => {
                    tracing::warn!(?path, %e, "failed to parse config, using defaults");
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(?path, "no config file, using defaults");
                Self::default()
            }
            Err(e) => {
                tracing::warn!(?path, %e, "failed to read config, using defaults");
                Self::default()
            }
        }
    }

    /// Serialize to YAML string (for `--dump-config`).
    pub fn to_yaml(&self) -> String {
        // Use serde_yaml for structure, then prepend header comment.
        let yaml = serde_yaml::to_string(self).unwrap_or_default();
        format!(
            "# wing configuration\n\
             # Generated by `wing tui --dump-config`\n\
             # Place at $WING_HOME/tui/config.yaml (default: ~/.wing/tui/config.yaml)\n\
             #\n\
             # Colors: `preset` picks the base palette — `Wing` (designed for\n\
             # dark terminals, the default) or `Terminal` (inherit the terminal's\n\
             # ANSI colours). Any other key under `colors:` overrides that one\n\
             # slot, e.g. `accent: \"#f0c674\"`; values are named ANSI colours\n\
             # (\"cyan\", \"dark_gray\") or hex (\"#00BCD4\"). Slots: accent, text,\n\
             # thinking, tool_result, dim, success, warning, danger, math,\n\
             # surface, diff_add_bg, diff_del_bg, diff_add_bg_strong,\n\
             # diff_del_bg_strong.\n\
             \n\
             {yaml}"
        )
    }
}

/// Resolved theme palette — `Color` values ready for rendering.
#[derive(Debug, Clone, Copy)]
pub struct ThemePalette {
    pub accent: Color,
    pub text: Color,
    pub thinking: Color,
    pub tool_result: Color,
    pub dim: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub surface: Color,
    /// Diff addition row background.
    pub diff_add_bg: Color,
    /// Diff deletion row background.
    pub diff_del_bg: Color,
    /// Word-level emphasis background inside an added line.
    pub diff_add_bg_strong: Color,
    /// Word-level emphasis background inside a deleted line.
    pub diff_del_bg_strong: Color,
    /// Math: rendered formulas and their literal fallback.
    pub math: Color,
    /// Math rendering mode (see [`MathMode`]).
    pub math_mode: MathMode,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self::from_config(&ColorsConfig::default())
    }
}

impl ThemePalette {
    /// Build palette from config: the preset's base, with per-slot overrides.
    ///
    /// An invalid colour string warns and falls back to the preset value for
    /// that slot — never fails the load.
    pub fn from_config(cfg: &ColorsConfig) -> Self {
        let base = cfg.preset.base();
        Self {
            accent: resolve(cfg.accent.as_deref(), base.accent),
            text: resolve(cfg.text.as_deref(), base.text),
            thinking: resolve(cfg.thinking.as_deref(), base.thinking),
            tool_result: resolve(cfg.tool_result.as_deref(), base.tool_result),
            dim: resolve(cfg.dim.as_deref(), base.dim),
            success: resolve(cfg.success.as_deref(), base.success),
            warning: resolve(cfg.warning.as_deref(), base.warning),
            danger: resolve(cfg.danger.as_deref(), base.danger),
            surface: resolve(cfg.surface.as_deref(), base.surface),
            diff_add_bg: resolve(cfg.diff_add_bg.as_deref(), base.diff_add_bg),
            diff_del_bg: resolve(cfg.diff_del_bg.as_deref(), base.diff_del_bg),
            diff_add_bg_strong: resolve(cfg.diff_add_bg_strong.as_deref(), base.diff_add_bg_strong),
            diff_del_bg_strong: resolve(cfg.diff_del_bg_strong.as_deref(), base.diff_del_bg_strong),
            math: resolve(cfg.math.as_deref(), base.math),
            math_mode: cfg.math_mode,
        }
    }
}

fn resolve(value: Option<&str>, fallback: Color) -> Color {
    match value {
        None => fallback,
        Some(v) => parse_color(v).unwrap_or_else(|| {
            tracing::warn!("invalid color '{v}', using the preset value");
            fallback
        }),
    }
}

/// Get the config file path.
///
/// Returns `None` if the home directory cannot be determined (the caller
/// should fall back to defaults).
fn config_path() -> Option<PathBuf> {
    let home = if let Ok(env_home) = std::env::var("WING_HOME") {
        PathBuf::from(env_home)
    } else {
        dirs::home_dir()?.join(".wing")
    };
    Some(home.join("tui").join("config.yaml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default preset is `Wing`, and its neutral ramp is strictly
    /// ordered — text > thinking > tool_result > dim, with `dim` and
    /// `tool_result` distinct values (the pre-preset default had one gray
    /// doing both jobs).
    #[test]
    fn test_default_palette_is_the_wing_preset_with_an_ordered_ramp() {
        assert_eq!(ColorsConfig::default().preset, ColorPreset::Wing);
        let p = ThemePalette::default();
        let luma = |c: Color| -> f32 {
            match c {
                Color::Rgb(r, g, b) => 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32,
                other => panic!("wing palette must be hex, got {other:?}"),
            }
        };
        assert!(luma(p.text) > luma(p.thinking), "text > thinking");
        assert!(
            luma(p.thinking) > luma(p.tool_result),
            "thinking > tool_result"
        );
        assert!(luma(p.tool_result) > luma(p.dim), "tool_result > dim");
        assert_ne!(p.dim, p.tool_result, "the two grays must differ");
        assert_eq!(p.math_mode, MathMode::Text);
    }

    /// `terminal` keeps the terminal's own ANSI colours — the pre-preset
    /// behaviour, slot for slot.
    #[test]
    fn test_terminal_preset_keeps_the_named_ansi_colours() {
        let cfg: AppConfig = serde_yaml::from_str("colors:\n  preset: terminal\n").unwrap();
        let p = ThemePalette::from_config(&cfg.colors);
        assert_eq!(p.accent, Color::Cyan);
        assert_eq!(p.text, Color::White);
        assert_eq!(p.thinking, Color::Gray);
        assert_eq!(p.tool_result, Color::DarkGray);
        assert_eq!(p.dim, Color::DarkGray);
        assert_eq!(p.success, Color::Green);
        assert_eq!(p.warning, Color::Yellow);
        assert_eq!(p.danger, Color::Red);
        assert_eq!(p.surface, Color::Rgb(52, 53, 65));
    }

    /// An unknown preset warns and keeps the default instead of failing the
    /// parse (same policy as the rendering modes).
    #[test]
    fn test_unknown_preset_falls_back_to_wing() {
        let cfg: AppConfig = serde_yaml::from_str("colors:\n  preset: neon\n").unwrap();
        assert_eq!(cfg.colors.preset, ColorPreset::Wing);
    }

    /// A slot key overrides exactly that slot; everything else follows the
    /// preset.
    #[test]
    fn test_slot_override_sits_on_the_preset() {
        let cfg: ColorsConfig = serde_yaml::from_str("accent: \"#ff00ff\"\ndim: gray\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.accent, Color::Rgb(255, 0, 255));
        assert_eq!(p.dim, Color::Gray);
        assert_eq!(p.text, ColorPreset::Wing.base().text);
    }

    /// The preset and its overrides round-trip through a dump: `Wing` /
    /// `Terminal` spell variant names that parse back, and the dump carries
    /// the override without noise for untouched slots.
    #[test]
    fn test_preset_and_overrides_round_trip_through_a_dump() {
        let mut cfg = AppConfig::default();
        cfg.colors.preset = ColorPreset::Terminal;
        cfg.colors.accent = Some("#ff00ff".into());
        let dumped = cfg.to_yaml();
        assert!(dumped.contains("preset: Terminal"), "{dumped}");
        let parsed: AppConfig = serde_yaml::from_str(&dumped).unwrap();
        assert_eq!(parsed.colors.preset, ColorPreset::Terminal);
        assert_eq!(parsed.colors.accent.as_deref(), Some("#ff00ff"));
        assert_eq!(parsed.colors.dim, None);
    }

    /// `rendering.math` reaches the palette: `resolve()` folds it into the
    /// carrier field `ThemePalette::from_config` reads.
    #[test]
    fn test_math_mode_reaches_the_palette() {
        let yaml = "rendering:\n  math: off\ncolors:\n  math: magenta\n";
        let mut cfg: AppConfig = serde_yaml::from_str(yaml).unwrap();
        cfg.resolve();
        let p = ThemePalette::from_config(&cfg.colors);
        assert_eq!(p.math_mode, MathMode::Off);
        assert_eq!(p.math, Color::Magenta);

        // The default config resolves to the default mode.
        let mut cfg = AppConfig::default();
        cfg.resolve();
        assert_eq!(
            ThemePalette::from_config(&cfg.colors).math_mode,
            MathMode::Text
        );
    }

    /// `math_mode` is a resolution carrier, not a YAML key of the colors
    /// section: a config that (wrongly) sets `colors.math_mode` cannot flip
    /// the renderer, and `--dump-config` never emits it there.
    #[test]
    fn test_math_mode_is_skipped_in_the_colors_section() {
        let yaml = "colors:\n  math_mode: off\n";
        let cfg: AppConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.colors.math_mode, MathMode::Text);
        let dumped = cfg.to_yaml();
        assert!(dumped.contains("math: Text"), "dumped config: {dumped}");
        assert_eq!(dumped.matches("math_mode").count(), 0);
    }

    /// A pre-redesign config (flat slot values under `colors:`, no `preset`,
    /// no diff tints) still loads: the values become overrides, missing keys
    /// follow the preset, and nothing fails the parse.
    #[test]
    fn test_legacy_config_without_preset_or_diff_tints() {
        let yaml = "colors:\n  accent: cyan\n  danger: red\n";
        let cfg: AppConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.colors.preset, ColorPreset::Wing);
        let p = ThemePalette::from_config(&cfg.colors);
        assert_eq!(p.accent, Color::Cyan);
        assert_eq!(p.danger, Color::Red);
        assert_eq!(p.diff_add_bg, ColorPreset::Wing.base().diff_add_bg);
    }

    /// An invalid colour string warns and keeps the preset value for that
    /// slot — never fails the load.
    #[test]
    fn test_invalid_color_falls_back_to_the_preset_slot() {
        let cfg = ColorsConfig {
            accent: Some("neon_pink".into()),
            ..ColorsConfig::default()
        };
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.accent, ColorPreset::Wing.base().accent);
    }

    #[test]
    fn test_default_config_yaml_roundtrip() {
        let cfg = AppConfig::default();
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let parsed: AppConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.colors.preset, ColorPreset::Wing);
        assert_eq!(parsed.colors.accent, None);
        assert_eq!(parsed.layout.max_input_lines, 10);
    }
}
