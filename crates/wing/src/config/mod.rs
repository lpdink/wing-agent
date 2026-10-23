//! User configuration — load from `$WING_HOME/tui/config.yaml`.
//!
//! Missing file → silent defaults. Parse error → warn + defaults.

pub mod catalog;
pub mod colors;
pub mod paste;
pub mod rendering;
pub mod store;

use std::path::Path;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
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
    /// Detect the terminal's background at startup and pick [`Self::Wing`] or
    /// [`Self::Light`]. No answer (tmux, screen, some SSH paths) falls back to
    /// `Wing` — the status quo, never a guess. Default.
    #[default]
    Auto,
    /// The designed palette for dark terminals.
    Wing,
    /// The designed palette for light terminals: the same hue families as
    /// `Wing`, re-solved against a white background.
    Light,
    /// Inherit the terminal's own ANSI colours — the pre-preset behaviour, for
    /// setups that follow an explicit terminal theme.
    Terminal,
}

impl<'de> Deserialize<'de> for ColorPreset {
    /// Case-insensitive, mirroring the other mode keys: `--dump-config`
    /// writes the variant names (`Auto` / `Wing` / `Light` / `Terminal`) and
    /// must round-trip. An unknown value warns and keeps the default instead
    /// of failing the whole config parse.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match Self::parse(&s) {
            Some(preset) => Ok(preset),
            None => {
                tracing::warn!("invalid color preset '{s}', falling back to 'auto'");
                Ok(Self::Auto)
            }
        }
    }
}

impl ColorPreset {
    /// Every preset, in the order the settings panel lists them. The single
    /// source the catalog choices, `theme_preview --preset` and the tests
    /// derive from — adding a variant here forces the other tables to notice.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Wing, Self::Light, Self::Terminal];

    /// The lowercase spelling used by the config file, the setting choices and
    /// the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Wing => "wing",
            Self::Light => "light",
            Self::Terminal => "terminal",
        }
    }

    /// Case-insensitive parse; `None` for an unknown spelling.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "wing" => Some(Self::Wing),
            "light" => Some(Self::Light),
            "terminal" => Some(Self::Terminal),
            _ => None,
        }
    }
}

/// The palette's own light/dark property — **explicit**, never inferred from
/// a slot. Shimmer sweep direction, the wordmark gradient, and the derived
/// heading rung all read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    /// Express "quieter than the surrounding text" for this palette.
    ///
    /// `Modifier::DIM` pushes the glyph toward the background: on a dark
    /// palette that is a step down; on a light one it washes an already-quiet
    /// hue out (and the terminal's own dimming may also shift the hue). So the
    /// intent is applied on dark and left to the slot's own colour on light —
    /// the single rule every "secondary" style (todo marks, quote bar, image
    /// captions) goes through.
    pub fn quiet(self, style: Style) -> Style {
        match self {
            Self::Dark => style.add_modifier(Modifier::DIM),
            Self::Light => style,
        }
    }
}

/// Which syntect theme code blocks use. Paired with [`Appearance`] by the
/// preset: a light-background syntax theme has to ship with a light palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxTheme {
    Dark,
    Light,
}

impl ColorPreset {
    /// Every slot's base value, before per-slot overrides.
    ///
    /// `Wing` assumes a dark terminal background — a cool neutral ramp
    /// (text → thinking → tool_result → dim) around a cyan accent, with the
    /// status colours pulled toward the brand's navy/amber family. `Light` is
    /// the same hue families re-solved against a white background (body text
    /// keeps a wide margin above WCAG 4.5, so the ramp survives anti-aliasing
    /// and non-sRGB displays). `Terminal` leaves the named ANSI colours in so
    /// the terminal theme decides — its body text is `Reset` (the terminal's
    /// default foreground), because ANSI `White` is the *background-adjacent*
    /// colour on many light schemes. `Auto` never reaches here: it is resolved
    /// to `Wing`/`Light` by [`ColorsConfig::resolved_preset`] first.
    fn base(self) -> ThemePalette {
        // The structure hue is one value across the slots that carry it (a
        // formula is structure like a heading), so it is named once.
        let violet = Color::Rgb(0xb4, 0x9a, 0xf0);
        let diff_add_bg = Color::Rgb(0x16, 0x38, 0x1f);
        let diff_del_bg = Color::Rgb(0x47, 0x24, 0x2c);
        let diff_add_bg_strong = Color::Rgb(0x1f, 0x52, 0x30);
        let diff_del_bg_strong = Color::Rgb(0x66, 0x32, 0x3c);
        match self {
            // Defensive: `Auto` is resolved before `base` is called.
            Self::Auto | Self::Wing => ThemePalette {
                accent: Color::Rgb(0x3a, 0xc6, 0xe0),
                text: Color::Rgb(0xe6, 0xea, 0xf2),
                thinking: Color::Rgb(0xa5, 0xae, 0xc1),
                tool_result: Color::Rgb(0x87, 0x91, 0xa7),
                dim: Color::Rgb(0x5b, 0x64, 0x78),
                success: Color::Rgb(0x4e, 0xc2, 0x8b),
                warning: Color::Rgb(0xe4, 0xb0, 0x4e),
                danger: Color::Rgb(0xe5, 0x64, 0x6e),
                math: violet,
                heading: violet,
                heading_bright: stretch(violet, Appearance::Dark),
                inline_code: Color::Rgb(0xe5, 0x78, 0x7f),
                marker: Color::Rgb(0x6f, 0x7d, 0xb4),
                quote: Color::Rgb(0x8f, 0x93, 0xc4),
                thinking_label: Color::Rgb(0xc9, 0x9a, 0x5e),
                strong: Color::Rgb(0xe5, 0xc0, 0x7b),
                surface: Color::Rgb(0x23, 0x2d, 0x42),
                diff_add_bg,
                diff_del_bg,
                diff_add_bg_strong,
                diff_del_bg_strong,
                math_mode: MathMode::Text,
                appearance: Appearance::Dark,
                syntax: SyntaxTheme::Dark,
                owns_background: true,
            },
            Self::Light => ThemePalette {
                accent: Color::Rgb(0x0e, 0x74, 0x90),
                text: Color::Rgb(0x1f, 0x23, 0x28),
                thinking: Color::Rgb(0x4b, 0x55, 0x63),
                tool_result: Color::Rgb(0x57, 0x60, 0x6a),
                dim: Color::Rgb(0x62, 0x6a, 0x73),
                success: Color::Rgb(0x1a, 0x7f, 0x37),
                warning: Color::Rgb(0x8a, 0x5a, 0x00),
                danger: Color::Rgb(0xb4, 0x23, 0x18),
                math: Color::Rgb(0x6d, 0x28, 0xd9),
                heading: Color::Rgb(0x6d, 0x28, 0xd9),
                heading_bright: Color::Rgb(0x5b, 0x21, 0xb6),
                inline_code: Color::Rgb(0xa4, 0x13, 0x3c),
                marker: Color::Rgb(0x5b, 0x6b, 0x8c),
                quote: Color::Rgb(0x64, 0x6b, 0x76),
                thinking_label: Color::Rgb(0x8a, 0x5a, 0x00),
                strong: Color::Rgb(0x7c, 0x4a, 0x03),
                surface: Color::Rgb(0xe7, 0xec, 0xf3),
                diff_add_bg: Color::Rgb(0xe6, 0xf4, 0xea),
                diff_del_bg: Color::Rgb(0xfd, 0xec, 0xec),
                diff_add_bg_strong: Color::Rgb(0xc6, 0xea, 0xd0),
                diff_del_bg_strong: Color::Rgb(0xf9, 0xc9, 0xc9),
                math_mode: MathMode::Text,
                appearance: Appearance::Light,
                syntax: SyntaxTheme::Light,
                owns_background: true,
            },
            Self::Terminal => ThemePalette {
                accent: Color::Cyan,
                // ANSI 15 ("bright white") is the *background-adjacent* colour
                // on light schemes (Solarized Light #fdf6e3, Tomorrow #ffffff):
                // body text must be the terminal's default foreground instead.
                text: Color::Reset,
                thinking: Color::Gray,
                tool_result: Color::DarkGray,
                dim: Color::DarkGray,
                success: Color::Green,
                warning: Color::Yellow,
                danger: Color::Red,
                math: Color::LightCyan,
                // A real ladder: level 1 brighter than level 2 (the Wing preset
                // splits them by luminance; named colours cannot be stretched).
                heading: Color::Magenta,
                heading_bright: Color::LightMagenta,
                strong: Color::LightGreen,
                inline_code: Color::LightRed,
                marker: Color::Blue,
                quote: Color::LightBlue,
                thinking_label: Color::LightYellow,
                // Reset = the terminal's own background: the card and the diff
                // tint bands merge into it instead of painting a fixed dark
                // slab on a light scheme. `+`/`-` markers and syntax colours
                // still carry the change.
                surface: Color::Reset,
                diff_add_bg: Color::Reset,
                diff_del_bg: Color::Reset,
                diff_add_bg_strong: Color::Reset,
                diff_del_bg_strong: Color::Reset,
                math_mode: MathMode::Text,
                // The terminal owns the actual appearance; `Dark` keeps the
                // pre-existing behaviour (sweep toward white) for this escape
                // hatch. Light-terminal users get `light` / `auto` instead.
                appearance: Appearance::Dark,
                syntax: SyntaxTheme::Dark,
                owns_background: false,
            },
        }
    }
}

/// What one color slot governs — written **once**, here. The settings panel and
/// `wing config` render this table; [`ColorsConfig`]'s fields deliberately do
/// not repeat the copy, so there is no second version to drift.
pub(crate) mod slot_doc {
    pub(crate) const ACCENT: &str = "Brand color: logo, commands, links, popups.";
    pub(crate) const TEXT: &str = "Primary text: messages, model name, input.";
    pub(crate) const THINKING: &str =
        "Thinking: agent reasoning content (more prominent than dim).";
    pub(crate) const THINKING_LABEL: &str =
        "Thinking label: the `⦁ 深度思考中 4s` summary line (and its sweep base).";
    pub(crate) const TOOL_RESULT: &str = "Tool result: tool call output (secondary data).";
    pub(crate) const DIM: &str = "Quiet register: borders, gutters, separators — and tool-call \
                                  args / timers (secondary metadata).";
    pub(crate) const SUCCESS: &str = "Success: diff+, token low.";
    pub(crate) const WARNING: &str = "Warning: pending, token mid.";
    pub(crate) const DANGER: &str = "Danger: diff-, error, token high.";
    pub(crate) const MATH: &str =
        "Math: rendered formulas (character grids) and their literal fallback.";
    pub(crate) const HEADING: &str =
        "Markdown headings (levels 2+; level 1 takes the derived `heading_bright`).";
    pub(crate) const STRONG: &str = "Emphasis (`**bold**`) in plain prose.";
    pub(crate) const INLINE_CODE: &str = "Inline code spans and unhighlighted code-block text.";
    pub(crate) const MARKER: &str =
        "List marks: bullets, ordered numbers, task boxes — and the table header band.";
    pub(crate) const QUOTE: &str = "Blockquote: the bar and the quoted voice.";
    pub(crate) const SURFACE: &str = "Surface: user message background.";
    pub(crate) const DIFF_ADD_BG: &str =
        "Background tint for diff additions (text keeps syntax colors).";
    pub(crate) const DIFF_DEL_BG: &str = "Background tint for diff deletions.";
    pub(crate) const DIFF_ADD_BG_STRONG: &str =
        "Background tint for changed words inside an added line.";
    pub(crate) const DIFF_DEL_BG_STRONG: &str =
        "Background tint for changed words inside a deleted line.";
}

/// Semantic color palette — one slot per UI role.
///
/// [`Self::preset`] picks the base palette; every other key overrides that
/// one slot (absent = follow the preset). `--dump-config` writes only the
/// preset and the overrides.
///
/// What each slot governs is written once, in [`slot_doc`] — the settings
/// panel and `wing config` render that same table, so the copy users read
/// cannot drift from the source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    /// Base palette (see [`ColorPreset`]).
    pub preset: ColorPreset,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub danger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub math: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strong: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_add_bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_del_bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_add_bg_strong: Option<String>,
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
/// This struct only contains TUI-specific settings (colors, layout, rendering, paste).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub colors: ColorsConfig,
    pub layout: LayoutConfig,
    pub rendering: RenderingConfig,
    /// 粘贴内容在发出前的处理（图片压缩参数）。
    pub paste: paste::PasteConfig,
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

impl AppConfig {
    /// Load configuration from `$WING_HOME/tui/config.yaml`.
    ///
    /// Returns defaults if the file doesn't exist or fails to parse.
    pub fn load() -> Self {
        let Some(path) = store::interface_config_path() else {
            tracing::warn!("cannot determine config directory, using defaults");
            return Self::default_resolved();
        };
        Self::load_from_path(&path)
    }

    /// Load from an explicit path — the implementation behind [`Self::load`].
    ///
    /// Also the seam the tests use to prove that `load` and [`store::appconfig_from_doc`] parse
    /// identically: both funnel into [`Self::from_doc`], so there is one parser, not two.
    pub(crate) fn load_from_path(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => match store::parse_document(&content, path) {
                Ok(doc) => match Self::from_doc_at(&doc, Some(path)) {
                    Some(cfg) => {
                        // 只有**真的按文档解析成功**才打这条 INFO（N-3：类型错误回落默认值时
                        // 打 "loaded config" 是误导）。
                        tracing::info!(?path, "loaded config");
                        cfg
                    }
                    // 类型错误：warn（带 path）已经由 `from_doc_at` 打出，这里只回落。
                    None => Self::default_resolved(),
                },
                Err(e) => {
                    tracing::warn!(?path, %e, "failed to parse config, using defaults");
                    Self::default_resolved()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(?path, "no config file, using defaults");
                Self::default_resolved()
            }
            Err(e) => {
                tracing::warn!(?path, %e, "failed to read config, using defaults");
                Self::default_resolved()
            }
        }
    }

    /// The shared parse: sparse (or full) document → `AppConfig`.
    ///
    /// Type errors warn and fall back to the defaults — never fails, matching the historical
    /// `load()` contract (an invalid value keeps the default instead of taking the TUI down).
    /// [`Self::resolve`] is applied here so every caller — including the settings panel's live
    /// preview via [`store::appconfig_from_doc`] — gets the same folded config `load()` returns.
    pub(crate) fn from_doc(doc: &serde_json::Value) -> Self {
        Self::from_doc_at(doc, None).unwrap_or_else(Self::default_resolved)
    }

    /// Fallback default for every error path — **`resolve()` 过**。
    ///
    /// 成功路径是「解析 → resolve」，回落路径必须是同一形态：`resolve()` 今天只折叠
    /// `colors.math_mode = rendering.math`，两边恰好相等，但默认值将来一旦偏离这个
    /// 等式，未 resolve 的回落就会造出「同一份默认配置、两种形态」的隐性分叉。
    fn default_resolved() -> Self {
        let mut cfg = Self::default();
        cfg.resolve();
        cfg
    }

    /// [`Self::from_doc`] with the source path for the log line (N-3).
    ///
    /// `Some(cfg)` = 文档按声明解析成功；`None` = 类型错误（warn 已打出，调用方回落默认值）。
    /// 三种启动状态的日志因此完整：文件不存在 = debug；解析 / 类型错误 = warn（都带 path）；
    /// 成功 = info——且成功那一栏只在真的成功时打。
    fn from_doc_at(doc: &serde_json::Value, path: Option<&Path>) -> Option<Self> {
        match serde_json::from_value::<Self>(doc.clone()) {
            Ok(mut cfg) => {
                cfg.resolve();
                Some(cfg)
            }
            Err(e) => {
                tracing::warn!(?path, %e, "failed to parse config, using defaults");
                None
            }
        }
    }

    /// Serialize this config to canonical YAML (section banners + commented defaults).
    ///
    /// A thin shell over [`catalog::dump_config_yaml`]: the emitter is catalog-driven, so the Rust
    /// side still has exactly one declaration. This is the *struct* dump (every key present with
    /// its value); the `--dump-config` CLI and the settings panel's save path go through
    /// [`store`] instead, because they dump a *document* — sparse, i.e. only the keys the user
    /// actually wrote, with everything else left as commented defaults.
    ///
    /// [`catalog::DumpMode::Raw`]: a struct dump is a round-trip artifact, so secrets are written
    /// verbatim (production has no caller; the one display surface — `wing tui --dump-config` —
    /// goes through `store`, which picks the mode explicitly). Do not wire this into a display
    /// path: it would print the real `api_key`.
    pub fn to_yaml(&self) -> String {
        match serde_json::to_value(self) {
            Ok(doc) => catalog::dump_config_yaml(&doc, catalog::DumpMode::Raw),
            Err(_) => String::new(),
        }
    }
}

/// Resolved theme palette — `Color` values ready for rendering.
#[derive(Debug, Clone, Copy)]
pub struct ThemePalette {
    pub accent: Color,
    pub text: Color,
    pub thinking: Color,
    /// The `⦁ 深度思考中 4s` summary line and the base of its shimmer sweep
    /// (the sweep peaks toward white).
    pub thinking_label: Color,
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
    /// Markdown headings (levels 2+; `heading_bright` is level 1).
    pub heading: Color,
    /// **Derived, not a config key**: the level-1 heading foreground — the top
    /// of the heading ladder, lifted from `heading` toward white.
    pub heading_bright: Color,
    /// Inline code spans and unhighlighted code-block text.
    pub inline_code: Color,
    /// List marks: bullets, ordered numbers, task boxes.
    pub marker: Color,
    /// Blockquote bar and quoted voice.
    pub quote: Color,
    /// Emphasis (`**bold**`) in plain prose: a 24-bit bold span does not
    /// brighten by itself (SGR 1 only changes the glyph weight), so emphasis
    /// carries a hue.
    pub strong: Color,
    /// Math rendering mode (see [`MathMode`]).
    pub math_mode: MathMode,
    /// The palette's own light/dark property. Rendering code that needs a
    /// direction (sweep peaks, brand gradients, derived rungs) reads this —
    /// never a colour slot, which a user override can flip.
    pub appearance: Appearance,
    /// Which syntect theme code blocks use, paired with `appearance`.
    pub syntax: SyntaxTheme,
    /// Whether this palette paints its own background bands (card surface, diff
    /// tints, toast). `terminal` hands the background to the terminal and sets
    /// this to `false` — a property of the preset, not something to infer from
    /// a colour slot.
    pub owns_background: bool,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self::from_config(&ColorsConfig::default())
    }
}

impl ThemePalette {
    /// Build palette from config: the preset's base, with per-slot overrides.
    ///
    /// `Auto` resolves against "no detection result" (→ `Wing`); callers that
    /// have probed the terminal use [`Self::from_config_with_appearance`].
    ///
    /// An invalid colour string warns and falls back to the preset value for
    /// that slot — never fails the load.
    pub fn from_config(cfg: &ColorsConfig) -> Self {
        Self::from_config_with_appearance(cfg, None)
    }

    /// [`Self::from_config`] with the result of the terminal probe (only read
    /// when `preset == Auto`).
    pub fn from_config_with_appearance(cfg: &ColorsConfig, detected: Option<Appearance>) -> Self {
        let base = cfg.resolved_preset(detected).base();
        let text = resolve(cfg.text.as_deref(), base.text);
        let heading = resolve(cfg.heading.as_deref(), base.heading);
        // The level-1 rung is a preset value, unless the user replaced
        // `heading` itself: then it is derived from the new hue, pushed along
        // the palette's own direction (a light palette deepens, a dark one
        // brightens). Reading the direction from `appearance` — not from a
        // colour slot — keeps a user override of `text` from flipping it.
        let heading_bright = match (cfg.heading.is_some(), heading) {
            (false, _) => base.heading_bright,
            (true, Color::Rgb(_, _, _)) => stretch(heading, base.appearance),
            (true, named) => named,
        };
        Self {
            accent: resolve(cfg.accent.as_deref(), base.accent),
            text,
            thinking: resolve(cfg.thinking.as_deref(), base.thinking),
            thinking_label: resolve(cfg.thinking_label.as_deref(), base.thinking_label),
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
            heading,
            heading_bright,
            inline_code: resolve(cfg.inline_code.as_deref(), base.inline_code),
            marker: resolve(cfg.marker.as_deref(), base.marker),
            quote: resolve(cfg.quote.as_deref(), base.quote),
            strong: resolve(cfg.strong.as_deref(), base.strong),
            math_mode: cfg.math_mode,
            appearance: base.appearance,
            syntax: base.syntax,
            owns_background: base.owns_background,
        }
    }
}

impl ThemePalette {
    /// Background band for a selected row.
    ///
    /// Normally the card surface. The `terminal` preset hands the background
    /// to the terminal (`surface == Reset`), and a `Reset` band would make a
    /// selected row indistinguishable from an unselected one — fall back to
    /// the quiet register, which is a real ANSI colour in that preset.
    pub fn selection_band(&self) -> Color {
        if self.owns_background {
            self.surface
        } else {
            self.dim
        }
    }
}

impl ColorsConfig {
    /// The concrete preset the palette is built from: `Auto` folds the probe
    /// result into `Wing`/`Light`, everything else passes through.
    ///
    /// `None` (no answer: tmux, screen, some SSH paths, a probe that never
    /// ran) keeps the status quo — `Wing`. Never guess a light background.
    pub fn resolved_preset(&self, detected: Option<Appearance>) -> ColorPreset {
        match self.preset {
            ColorPreset::Auto => match detected {
                Some(Appearance::Light) => ColorPreset::Light,
                _ => ColorPreset::Wing,
            },
            preset => preset,
        }
    }
}

/// Push an RGB colour 30% of the way toward the extreme that carries contrast:
/// white on a dark palette, black on a light one. The derived step behind
/// `heading_bright` (the level-1 rung).
///
/// Named colours are returned unchanged: they follow the terminal theme, which
/// already owns what "brighter" means, and there is no RGB to push.
fn stretch(color: Color, appearance: Appearance) -> Color {
    const STEP: f32 = 0.30;
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let target = match appearance {
        Appearance::Light => 0.0,
        Appearance::Dark => 255.0,
    };
    let push = |c: u8| -> u8 { (f32::from(c) + (target - f32::from(c)) * STEP).round() as u8 };
    Color::Rgb(push(r), push(g), push(b))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance (0 = black, 1 = white).
    fn wcag_luma(c: Color) -> f32 {
        let Color::Rgb(r, g, b) = c else {
            panic!("expected hex, got {c:?}");
        };
        let channel = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast_pair(a: Color, b: Color) -> f32 {
        let (la, lb) = (wcag_luma(a), wcag_luma(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// The default preset is `Auto`; with no probe result it resolves to
    /// `Wing`, whose neutral ramp is strictly ordered — text > thinking >
    /// tool_result > dim, with `dim` and `tool_result` distinct values (the
    /// pre-preset default had one gray doing both jobs).
    #[test]
    fn test_default_palette_is_the_wing_preset_with_an_ordered_ramp() {
        assert_eq!(ColorsConfig::default().preset, ColorPreset::Auto);
        let p = ThemePalette::default();
        assert_eq!(p.appearance, Appearance::Dark);
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
        assert_eq!(p.syntax, SyntaxTheme::Dark);
    }

    /// The light preset is the same ramp mirrored around a white background:
    /// darker = louder, so the ordering inverts and every body/info slot keeps
    /// a comfortable margin over WCAG 4.5:1.
    #[test]
    fn test_light_preset_is_readable_on_white() {
        let cfg: ColorsConfig = serde_yaml::from_str("preset: light\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.appearance, Appearance::Light);
        assert_eq!(p.syntax, SyntaxTheme::Light);
        // text < thinking < tool_result < dim: quieter slots are lighter.
        let text = wcag_luma(p.text);
        let thinking = wcag_luma(p.thinking);
        let tool_result = wcag_luma(p.tool_result);
        let dim = wcag_luma(p.dim);
        assert!(text < thinking, "text is the darkest rung");
        assert!(thinking < tool_result, "thinking < tool_result");
        assert!(tool_result < dim, "tool_result < dim");
        // Body and every information slot clear 4.5:1 on white **and on a
        // common light-grey terminal background** (#f7f7f7); quiet slots clear
        // 3.0 (the large/secondary line).
        for (name, color, line) in [
            ("text", p.text, 4.5),
            ("thinking", p.thinking, 4.5),
            ("tool_result", p.tool_result, 4.5),
            ("dim", p.dim, 4.5),
            ("accent", p.accent, 4.5),
            ("success", p.success, 4.5),
            ("warning", p.warning, 4.5),
            ("danger", p.danger, 4.5),
            ("math", p.math, 4.5),
            ("heading", p.heading, 4.5),
            ("heading_bright", p.heading_bright, 4.5),
            ("inline_code", p.inline_code, 4.5),
            ("quote", p.quote, 3.0),
            ("marker", p.marker, 3.0),
            ("thinking_label", p.thinking_label, 4.5),
            ("strong", p.strong, 4.5),
        ] {
            for bg in [Color::Rgb(255, 255, 255), Color::Rgb(0xf7, 0xf7, 0xf7)] {
                let ratio = contrast_pair(color, bg);
                assert!(
                    ratio >= line,
                    "{name} must clear {line}:1 on {bg:?}, got {ratio:.2}"
                );
            }
        }
        // The quiet register also has to survive on the card band (it is the
        // one slot that appears *inside* the surface and the diff tints).
        for (name, bg) in [
            ("surface", p.surface),
            ("diff_add_bg", p.diff_add_bg),
            ("diff_del_bg", p.diff_del_bg),
        ] {
            let ratio = contrast_pair(p.dim, bg);
            assert!(
                ratio >= 4.5,
                "dim on {name} must clear 4.5:1, got {ratio:.2}"
            );
        }
        // The card and diff tints stay quiet (they sit under body text), and
        // the text on them stays readable.
        for (name, bg) in [
            ("surface", p.surface),
            ("diff_add_bg", p.diff_add_bg),
            ("diff_del_bg", p.diff_del_bg),
            ("diff_add_bg_strong", p.diff_add_bg_strong),
            ("diff_del_bg_strong", p.diff_del_bg_strong),
        ] {
            let ratio = contrast_pair(p.text, bg);
            assert!(
                ratio >= 4.5,
                "text on {name} must clear 4.5:1, got {ratio:.2}"
            );
        }
    }

    /// `syntax` is currently a pure function of `appearance` (four presets,
    /// one pairing). Pin it: a preset that pairs a light palette with the dark
    /// syntax theme would be a silent, hard-to-see regression (dark code
    /// colours on white).
    #[test]
    fn every_preset_pairs_its_syntax_theme_with_its_appearance() {
        for preset in ColorPreset::ALL {
            let p = preset.base();
            let expected = match p.appearance {
                Appearance::Light => SyntaxTheme::Light,
                Appearance::Dark => SyntaxTheme::Dark,
            };
            assert_eq!(p.syntax, expected, "{preset:?}");
        }
    }

    /// `Auto` folds the probe result into a concrete preset: light → `Light`,
    /// dark/unknown → `Wing` (never a guess).
    #[test]
    fn test_auto_preset_resolves_from_the_probe() {
        let cfg = ColorsConfig::default();
        assert_eq!(
            cfg.resolved_preset(Some(Appearance::Light)),
            ColorPreset::Light
        );
        assert_eq!(
            cfg.resolved_preset(Some(Appearance::Dark)),
            ColorPreset::Wing
        );
        assert_eq!(cfg.resolved_preset(None), ColorPreset::Wing);
        // An explicit preset ignores the probe.
        let cfg: ColorsConfig = serde_yaml::from_str("preset: light\n").unwrap();
        assert_eq!(
            cfg.resolved_preset(Some(Appearance::Dark)),
            ColorPreset::Light
        );
    }

    /// `terminal` keeps the terminal's own ANSI colours — the pre-preset
    /// behaviour, slot for slot. Body text is the terminal's default
    /// foreground (`Reset`), not ANSI 15: on light schemes "bright white" is
    /// the background-adjacent colour.
    #[test]
    fn test_terminal_preset_keeps_the_named_ansi_colours() {
        let cfg: AppConfig = serde_yaml::from_str("colors:\n  preset: terminal\n").unwrap();
        let p = ThemePalette::from_config(&cfg.colors);
        assert_eq!(p.accent, Color::Cyan);
        assert_eq!(p.text, Color::Reset);
        assert_eq!(p.thinking, Color::Gray);
        assert_eq!(p.tool_result, Color::DarkGray);
        assert_eq!(p.dim, Color::DarkGray);
        assert_eq!(p.success, Color::Green);
        assert_eq!(p.warning, Color::Yellow);
        assert_eq!(p.danger, Color::Red);
        assert_eq!(p.heading, Color::Magenta);
        assert_eq!(p.heading_bright, Color::LightMagenta);
        assert_eq!(p.strong, Color::LightGreen);
        assert_eq!(p.inline_code, Color::LightRed);
        assert_eq!(p.thinking_label, Color::LightYellow);
        assert_eq!(p.marker, Color::Blue);
        assert_eq!(p.quote, Color::LightBlue);
        assert_eq!(p.math, Color::LightCyan);
        assert!(
            p.heading_bright != p.heading,
            "H1 must stay a rung above H2"
        );
        assert_ne!(p.marker, p.dim, "list marks must not read as chrome");
        // Background bands follow the terminal instead of painting a fixed
        // dark slab (the light-scheme failure this preset used to carry).
        assert_eq!(p.surface, Color::Reset);
        assert_eq!(p.diff_add_bg, Color::Reset);
    }

    /// An unknown preset warns and keeps the default instead of failing the
    /// parse (same policy as the rendering modes).
    #[test]
    fn test_unknown_preset_falls_back_to_auto() {
        let cfg: AppConfig = serde_yaml::from_str("colors:\n  preset: neon\n").unwrap();
        assert_eq!(cfg.colors.preset, ColorPreset::Auto);
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
        let dumped = crate::config::catalog::dump_config_yaml(
            &serde_json::to_value(&cfg).unwrap(),
            crate::config::catalog::DumpMode::Raw,
        );
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
        let dumped = crate::config::catalog::dump_config_yaml(
            &serde_json::to_value(&cfg).unwrap(),
            crate::config::catalog::DumpMode::Raw,
        );
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
        assert_eq!(cfg.colors.preset, ColorPreset::Auto);
        let p = ThemePalette::from_config(&cfg.colors);
        assert_eq!(p.accent, Color::Cyan);
        assert_eq!(p.danger, Color::Red);
        assert_eq!(p.diff_add_bg, ColorPreset::Wing.base().diff_add_bg);
    }

    /// The markdown role slots are real YAML keys: each overrides exactly its
    /// own slot, and the derived values follow the slot they are derived from.
    #[test]
    fn test_markdown_role_slots_override_and_drive_their_derived_partner() {
        let cfg: ColorsConfig = serde_yaml::from_str(
            "heading: \"#00ff00\"\ninline_code: yellow\nmarker: gray\nquote: \"#123456\"\n",
        )
        .unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.heading, Color::Rgb(0, 255, 0));
        assert_eq!(p.inline_code, Color::Yellow);
        assert_eq!(p.marker, Color::Gray);
        assert_eq!(p.quote, Color::Rgb(0x12, 0x34, 0x56));
        // `heading_bright` is the level-1 rung: it follows `heading`, and an
        // override moves it with it (a named colour cannot be lifted, so it
        // stays the same value).
        assert_ne!(p.heading_bright, p.heading, "hex heading must be lifted");
        let cfg: ColorsConfig = serde_yaml::from_str("heading: green\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.heading_bright, Color::Green);
    }

    /// `strong` is a slot of its own (emphasis needs a hue: a 24-bit bold span
    /// does not brighten by itself), and it is never the body colour.
    #[test]
    fn test_strong_is_its_own_slot() {
        let cfg: ColorsConfig = serde_yaml::from_str("strong: \"#00ff00\"\n").unwrap();
        assert_eq!(
            ThemePalette::from_config(&cfg).strong,
            Color::Rgb(0, 255, 0)
        );
        // Default: a warm gold, distinct from the body colour it emphasises.
        let p = ThemePalette::default();
        assert_ne!(p.strong, p.text);
        assert_ne!(p.strong, p.inline_code);
        assert_ne!(p.strong, p.heading);
    }

    /// `heading_bright` is the level-1 rung: a preset supplies it, and when the
    /// user replaces `heading` itself it is derived from the new hue along the
    /// **palette's** direction (`appearance`) — lift on a dark palette, deepen
    /// on a light one. Overriding `text` must not flip it: the direction is a
    /// palette property, not a colour-slot guess.
    #[test]
    fn test_heading_bright_follows_heading_and_the_palette_direction() {
        let luma = |c: Color| -> u16 {
            match c {
                Color::Rgb(r, g, b) => u16::from(r) + u16::from(g) + u16::from(b),
                other => panic!("expected hex, got {other:?}"),
            }
        };
        let dark = ThemePalette::default();
        assert!(luma(dark.heading_bright) > luma(dark.heading), "dark: lift");

        let cfg: ColorsConfig = serde_yaml::from_str("preset: light\n").unwrap();
        let light = ThemePalette::from_config(&cfg);
        assert!(
            luma(light.heading_bright) < luma(light.heading),
            "light: deepen in the preset itself"
        );

        // Overriding only `text` stays on the same palette: H1 keeps its rung.
        let cfg: ColorsConfig = serde_yaml::from_str("text: \"#202020\"\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.appearance, Appearance::Dark);
        assert_eq!(p.heading_bright, dark.heading_bright);

        // A hex heading override is derived along the preset's direction.
        let cfg: ColorsConfig = serde_yaml::from_str("heading: \"#808080\"\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert!(luma(p.heading_bright) > luma(p.heading), "dark: lift");
        let cfg: ColorsConfig =
            serde_yaml::from_str("preset: light\nheading: \"#808080\"\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert!(luma(p.heading_bright) < luma(p.heading), "light: deepen");

        // A named heading cannot be stretched: the value stands.
        let cfg: ColorsConfig = serde_yaml::from_str("heading: green\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.heading, Color::Green);
        assert_eq!(p.heading_bright, Color::Green);

        // The terminal preset keeps its explicit named ladder (H1 ≠ H2).
        let cfg: ColorsConfig = serde_yaml::from_str("preset: terminal\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.heading, Color::Magenta);
        assert_eq!(p.heading_bright, Color::LightMagenta);

        // An invalid value falls back to the preset slot, rung included.
        let cfg: ColorsConfig = serde_yaml::from_str("heading: neon_pink\n").unwrap();
        let p = ThemePalette::from_config(&cfg);
        assert_eq!(p.heading, dark.heading);
        assert_eq!(p.heading_bright, dark.heading_bright);
    }

    /// Every role is a distinct hue, and distinct *perceptually*: roles that
    /// sit on top of each other defeat the point of splitting them.
    ///
    /// One exemption is deliberate and documented
    /// (`docs/dev/tui-rendering.md` 二·八): inline code sits next to `danger`,
    /// because the choice of a rose for literals is the product decision — the
    /// two are told apart by context (prose vs a railed notice line), not by
    /// the swatch.
    #[test]
    fn test_the_default_roles_are_distinct_hues() {
        let p = ThemePalette::default();
        let roles = [
            ("text", p.text),
            ("heading", p.heading),
            ("strong", p.strong),
            ("inline_code", p.inline_code),
            ("accent", p.accent),
            ("marker", p.marker),
            ("quote", p.quote),
            ("tool_result", p.tool_result),
            ("dim", p.dim),
            ("thinking", p.thinking),
            ("thinking_label", p.thinking_label),
            ("success", p.success),
            ("warning", p.warning),
            ("danger", p.danger),
        ];
        // Manhattan distance in RGB — coarse, but it catches "same swatch".
        let distance = |a: Color, b: Color| -> u32 {
            match (a, b) {
                (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
                    u32::from(r1.abs_diff(r2))
                        + u32::from(g1.abs_diff(g2))
                        + u32::from(b1.abs_diff(b2))
                }
                _ => 0,
            }
        };
        for (i, (name_a, a)) in roles.iter().enumerate() {
            for (name_b, b) in roles.iter().skip(i + 1) {
                assert_ne!(a, b, "{name_a} and {name_b} are the same colour");
                let exempt = matches!(
                    (*name_a, *name_b),
                    ("inline_code", "danger") | ("danger", "inline_code")
                );
                if !exempt {
                    assert!(
                        distance(*a, *b) >= 30,
                        "{name_a} and {name_b} are too close ({a:?} vs {b:?})"
                    );
                }
            }
        }
        // Code and links ("one hue, one meaning") must not collapse; headings
        // and quotes are one family at different rungs; math rides the
        // heading rung.
        assert_ne!(p.inline_code, p.accent);
        assert_ne!(p.heading, p.quote);
        assert_eq!(p.math, p.heading);
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
        assert_eq!(parsed.colors.preset, ColorPreset::Auto);
        assert_eq!(parsed.colors.accent, None);
        assert_eq!(parsed.layout.max_input_lines, 10);
    }
}
