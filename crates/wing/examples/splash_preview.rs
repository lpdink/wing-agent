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
//! 状态是 fixture（不连网关）：会话 / 模型 / 用量 / 草稿 / 会话事实都走 `SyncSession`
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

use std::io::Write as _;

use wing::app::preview;
use wing::ui::html::page_colors;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match preview::Options::parse(&args) {
        Ok(options) => options,
        Err(message) => fail(&message, 2),
    };
    if options.help {
        emit(preview::USAGE);
        return;
    }

    let frame = preview::sample(&options.spec);
    let caption = options.caption(&frame);
    match &options.html {
        Some(path) => {
            let (default_bg, default_fg) = page_colors(frame.palette());
            let document = frame.html(
                &caption,
                options.background.as_deref().unwrap_or(default_bg),
                options.foreground.as_deref().unwrap_or(default_fg),
            );
            if let Err(e) = std::fs::write(path, document) {
                fail(&format!("cannot write {}: {e}", path.display()), 1);
            }
            note(&format!("{}  →  {caption}", path.display()));
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
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// 回执 / 说明行走 stderr：stdout 只留帧（重定向到文件、贴进 diff 时不掺东西）。
fn note(text: &str) {
    let _ = writeln!(std::io::stderr(), "{text}");
}

fn fail(message: &str, code: i32) -> ! {
    note(&format!("splash_preview: {message}"));
    std::process::exit(code);
}
