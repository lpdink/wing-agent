//! `splash_preview` — 开屏整块的真帧：状态栏 + 牌子 + 输入卡。
//!
//! 改开屏的视觉（名牌排版 / 输入卡的边框与轨道 / 状态栏断句 / 光学中心）时用它：
//! 一条命令拿到整屏，不用起网关、不用开 TUI、不用肉眼对齐。
//!
//! ```text
//! # 默认：100×30、配置里的明暗档、已定格（稳态那一帧）
//! cargo run -p wing --example splash_preview
//!
//! # 常用的三档：宽屏 2x 大字 / 100 列亮底 / 80 列（多数终端的默认宽）
//! cargo run -p wing --example splash_preview -- --width 120 --height 40 --preset dark
//! cargo run -p wing --example splash_preview -- --width 100 --height 36 --preset light
//! cargo run -p wing --example splash_preview -- --width 80  --height 24
//!
//! # 输出后端：无颜色（贴 issue / diff）· HTML（浏览器里对照、截图贴 PR）
//! cargo run -p wing --example splash_preview -- --width 80 --height 24 --plain
//! cargo run -p wing --example splash_preview -- --width 120 --height 40 \
//!     --html /tmp/splash.html
//!
//! # 会话状态：输入卡里有草稿 / agent 在干活（活动栏 + 飞行姿态）/ 扫光停在 40%
//! cargo run -p wing --example splash_preview -- --draft "把状态栏的断句改一下"
//! cargo run -p wing --example splash_preview -- --working
//! cargo run -p wing --example splash_preview -- --phase 0.4
//! ```
//!
//! 画的就是 TUI 里那一帧：`wing::app::preview` 造一个真 `App`、用真事件灌好状态、
//! 调**同一个** `App::draw` 画进 `TestBackend`，再把那块 `Buffer` 交回来 —— 输入卡的
//! 圆角框、两条轨道、光学中心、状态栏的截断顺序都是真代码算出来的，不是重画一份长得
//! 像的（为什么非这样不可，见 `app::preview` 的模块文档）。
//!
//! 状态是 fixture（不连网关）：会话 / 模型 / 用量 / 草稿 / 事实行都走 `SyncSession`
//! 等真事件，数字是固定的样例 —— 所以同一份代码两次运行逐字节相同，视觉改动的 diff
//! 里不会混进抽签噪声。调色板读真实配置（与 `theme_preview` 一致）：纯预设、不含
//! 个人颜色覆盖时把 `WING_HOME` 指到空目录再跑
//! （`WING_HOME=/tmp/empty-wing cargo run -p wing --example splash_preview`）。
//!
//! ## 改视觉的推荐工作流
//!
//! 1. 先留基线：`--plain` 存一份（`/tmp/before.txt`），改完再存一份，`diff` 看几何
//!    有没有动（分节线长度、输入卡宽度、键位行是否折了）。
//! 2. 三档宽度都过一遍：40 / 80 / 120。80 列是最常见的终端，40 列会掉到最窄档。
//! 3. 明暗两套都过一遍（`--preset dark` / `--preset light`）：亮底上淡色的
//!    `surface` / `dim` 是重灾区。
//! 4. 要贴给别人看：`--html /tmp/x.html`，页面底色按 appearance 补，浏览器里就是
//!    终端里那一帧。
//! 5. 几何回归由 `app::tests::splash_frame` 的整屏指纹兜底（牌子的绝对位置、分节线
//!    长度、输入卡边框行、状态栏两端）—— 改版式常量时要一起更新断言。
//!
//! 不覆盖：命令弹层 / 设置面板 / toast / 图片（它们是浮层，取样面只到开屏整块）。

use std::path::PathBuf;

use wing::app::preview;
use wing::app::preview::Spec;
use wing::config::Appearance;
use wing::config::ColorPreset;
use wing::ui::html::page_colors;
use wing::ui::welcome::SessionFacts;

const USAGE: &str = "\
splash_preview — 开屏整块的真帧（状态栏 + 牌子 + 输入卡）

USAGE:
    cargo run -p wing --example splash_preview [OPTIONS]

OPTIONS:
    --width <N>         终端列数（默认 100）
    --height <N>        终端行数（默认 30）—— 开屏的竖直位置由它决定
    --preset <NAME>     auto | dark | light（默认沿用配置）
    --draft <TEXT>      输入卡里放一段草稿（默认空：显示占位文案）
    --working           agent 在干活：活动栏 + 飞行姿态 + 这一轮的用量
    --facts <S,R>       名牌右列的会话事实（默认 2,1；0,0 = 这一行留空）
    --phase <0..1>      开屏扫光的相位（默认已定格；1 = 已扫完，同定格）
    --width/--height 上限 1000
    --plain             无颜色输出（半格像素还原成实心块）
    --html <PATH>       写出 HTML（浏览器里对照 / 截图），与 --plain 互斥
    --bg <#RRGGBB>      HTML 页面底色（默认按明暗档）
    --fg <#RRGGBB>      HTML 页面前景色
    -h, --help          这一页

    `--flag=value` 是等号形态：值本身以 `--` 开头时（例如草稿里带 `--`）用它。
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match Options::parse(&args) {
        Ok(options) => options,
        Err(message) => fail(&message, 2),
    };
    if options.help {
        emit(USAGE);
        return;
    }

    let frame = preview::sample(&options.spec);
    let caption = caption(&frame, &options.spec);
    match options.html {
        Some(path) => {
            let (default_bg, default_fg) = page_colors(frame.palette());
            let document = frame.html(
                &caption,
                options.background.as_deref().unwrap_or(default_bg),
                options.foreground.as_deref().unwrap_or(default_fg),
            );
            if let Err(e) = std::fs::write(&path, document) {
                fail(&format!("cannot write {}: {e}", path.display()), 1);
            }
            note(&format!("{}  →  {}", path.display(), caption));
        }
        None => {
            note(&caption);
            let body = if options.plain {
                frame.plain()
            } else {
                frame.ansi()
            };
            emit(&body);
        }
    }
}

/// stdout 串行写口：管道提前关闭（`| head`）不是错误，不该 panic。
fn emit(text: &str) {
    use std::io::Write as _;
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// 一帧的说明行：尺寸 / 明暗 / 相位 / 状态，终端与 HTML 共用。
fn caption(frame: &preview::Frame, spec: &Spec) -> String {
    let appearance = match frame.palette().appearance {
        Appearance::Dark => "dark",
        Appearance::Light => "light",
    };
    let mut parts = vec![
        format!("{}×{}", spec.width, spec.height),
        appearance.to_string(),
        // `phase = 1` 与定格是同一帧（扫光窗口已经走完），说明行不该说"100%"。
        match spec.phase {
            Some(phase) if phase < 1.0 => format!("sweep {:.0}%", phase * 100.0),
            _ => "settled".to_string(),
        },
    ];
    parts.push(format!("facts {},{}", spec.facts.skills, spec.facts.rules));
    if spec.draft.is_some() {
        parts.push("draft".to_string());
    }
    if spec.working {
        parts.push("working".to_string());
    }
    parts.join(" · ")
}

// ── CLI ─────────────────────────────────────────────────────────

struct Options {
    spec: Spec,
    plain: bool,
    html: Option<PathBuf>,
    background: Option<String>,
    foreground: Option<String>,
    help: bool,
}

impl Options {
    /// `--flag value` 与 `--flag=value` 两种形态。
    ///
    /// `--flag --other` 是**用法错误**而不是"把 `--other` 当值"（那会静默写出一个
    /// 名叫 `--plain` 的文件）；值本身真以 `--` 开头时用等号形态。
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            spec: Spec::default(),
            plain: false,
            html: None,
            background: None,
            foreground: None,
            help: false,
        };
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
                _ => (arg, None),
            };
            let value = |index: &mut usize| -> Result<String, String> {
                if let Some(value) = inline.clone() {
                    return Ok(value);
                }
                match args.get(*index + 1) {
                    Some(next) if !next.starts_with("--") => {
                        *index += 1;
                        Ok(next.clone())
                    }
                    Some(next) => Err(format!(
                        "{flag} needs a value, got the flag '{next}' (write {flag}=<value> \
                         when the value itself starts with --)"
                    )),
                    None => Err(format!("{flag} needs a value")),
                }
            };
            match flag {
                "--help" | "-h" => options.help = true,
                "--plain" => options.plain = true,
                "--working" => options.spec.working = true,
                "--width" => options.spec.width = number(&value(&mut index)?, flag)?,
                "--height" => options.spec.height = number(&value(&mut index)?, flag)?,
                "--preset" => {
                    let raw = value(&mut index)?;
                    options.spec.preset = Some(ColorPreset::parse(&raw).ok_or_else(|| {
                        format!(
                            "unknown preset '{raw}' (known: {})",
                            ColorPreset::ALL
                                .iter()
                                .map(|preset| preset.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?);
                }
                "--draft" => options.spec.draft = Some(value(&mut index)?),
                "--facts" => options.spec.facts = facts(&value(&mut index)?)?,
                "--phase" => {
                    let raw = value(&mut index)?;
                    let phase = raw
                        .parse::<f32>()
                        .ok()
                        .filter(|phase| (0.0..=1.0).contains(phase))
                        .ok_or_else(|| format!("--phase expects a number in 0..=1, got '{raw}'"))?;
                    options.spec.phase = Some(phase);
                }
                "--html" => options.html = Some(PathBuf::from(value(&mut index)?)),
                "--bg" => options.background = Some(value(&mut index)?),
                "--fg" => options.foreground = Some(value(&mut index)?),
                other if other.starts_with('-') => {
                    return Err(format!("unknown flag '{other}' (see --help)"));
                }
                other => return Err(format!("unexpected argument '{other}' (see --help)")),
            }
            index += 1;
        }

        if options.plain && options.html.is_some() {
            return Err(
                "--plain and --html are different outputs: write the frame to stdout or to a \
                 styled HTML file, not both"
                    .into(),
            );
        }
        if (options.background.is_some() || options.foreground.is_some()) && options.html.is_none()
        {
            return Err("--bg / --fg only apply to --html".into());
        }
        Ok(options)
    }
}

/// 正整数的列 / 行数。上界是防呆：`--width 65535 --height 65535` 会让取样器去分配
/// 四十亿个格子（卡死或 OOM），而不是报错。
fn number(raw: &str, flag: &str) -> Result<u16, String> {
    raw.parse::<u16>()
        .ok()
        .filter(|value| (1..=1000).contains(value))
        .ok_or_else(|| format!("{flag} expects a positive integer (max 1000), got '{raw}'"))
}

/// `--facts 2,1` → 会话事实（skills,rules）。
fn facts(raw: &str) -> Result<SessionFacts, String> {
    let (skills, rules) = raw
        .split_once(',')
        .ok_or_else(|| format!("--facts expects '<skills>,<rules>', got '{raw}'"))?;
    let parse = |text: &str, name: &str| -> Result<usize, String> {
        text.trim()
            .parse::<usize>()
            .map_err(|_| format!("--facts: {name} expects a count, got '{text}'"))
    };
    Ok(SessionFacts {
        skills: parse(skills, "skills")?,
        rules: parse(rules, "rules")?,
    })
}

/// 回执 / 说明行走 stderr：stdout 只留帧（重定向到文件、贴进 diff 时不掺东西）。
fn note(text: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{text}");
}

fn fail(message: &str, code: i32) -> ! {
    note(&format!("splash_preview: {message}"));
    std::process::exit(code);
}
