//! `export_logo` — 把欢迎屏的像素网格导出成品牌资产（SVG）。
//!
//! ```text
//! cargo run -p wing --example export_logo            # 写到 assets/
//! cargo run -p wing --example export_logo -- /tmp/x  # 写到别处
//! ```
//!
//! 产出三份（README 页头用前两份，`<picture>` 按 GitHub 主题切换）：
//!
//! * `banner-dark.svg` / `banner-light.svg` —— 飞行海鸥 + `WING` 大字横版 lockup；
//! * `gull.svg` —— 站姿 mascot 单只（docs / favicon 底稿）。
//!
//! **同一份数据源**：读的就是 [`wing::ui::welcome::art`]（引擎渲染终端的那几个
//! 网格）与 [`wing::ui::welcome::wordmark::column_color`]（终端里那套渐变）。
//! 改画之后重跑本 example，资产自动跟上 —— 不存在"README 和真机漂移"。
//!
//! 社交预览图（`assets/social-preview.png`，1280×640）需要浏览器渲染，不在本
//! example 里：把 banner 贴进一个 1280×640 的深色页面，用 headless Chrome 截
//! `--window-size=1280,640` 即可（见 `docs/dev/welcome-mascot.md`）。

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use wing::ui::welcome::art;
use wing::ui::welcome::sprite;
use wing::ui::welcome::sprite::Rgb;
use wing::ui::welcome::wordmark;

/// 品牌点缀色 —— 与终端默认主题的 accent 一致（cyan）。
const ACCENT: Rgb = (34, 211, 238);

/// 大字按几倍像素画：2 倍是"笔画不散架"的极限，3 倍起 1px 笔画会被拉开成虚线。
const WORDMARK_PIX: usize = 2;

/// 一只海鸥与 wordmark 之间的间隔像素。
const GAP: usize = 5;
/// lockup 四周留白。
const PAD: usize = 4;
/// SVG 的显示倍率（viewBox 里 1 单位 = 1 个精灵像素）。
const SCALE: usize = 6;

/// 一条矩形：`(x, y, w, h, color)`。
type Rect = (usize, usize, usize, usize, String);

fn hex(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
}

/// 字母网格 → rect：行内合并同名同色的连续段。
fn sprite_rects(grid: &[&str], x0: usize, y0: usize) -> Vec<Rect> {
    let mut out = Vec::new();
    for (y, row) in grid.iter().enumerate() {
        let chars: Vec<char> = row.chars().collect();
        let mut x = 0;
        while x < chars.len() {
            let Some(rgb) = sprite::brand(chars[x], ACCENT) else {
                x += 1;
                continue;
            };
            let color = hex(rgb);
            let start = x;
            while x < chars.len()
                && sprite::brand(chars[x], ACCENT).map(hex).as_deref() == Some(color.as_str())
            {
                x += 1;
            }
            out.push((x0 + start, y0 + y, x - start, 1, color));
        }
    }
    out
}

/// 像素大字 → rect（每格 `WORDMARK_PIX` 见方；横向渐变与终端同源）。
fn wordmark_rects(light: bool, x0: usize, y0: usize) -> (Vec<Rect>, usize, usize) {
    let mut out = Vec::new();
    let cols = art::WORDMARK_COLS;
    for (y, row) in art::WORDMARK.iter().enumerate() {
        for (x, ch) in row.chars().enumerate() {
            if ch != '#' {
                continue;
            }
            let rgb = wordmark::column_color(x, ACCENT, light);
            out.push((
                x0 + x * WORDMARK_PIX,
                y0 + y * WORDMARK_PIX,
                WORDMARK_PIX,
                WORDMARK_PIX,
                hex(rgb),
            ));
        }
    }
    (out, cols * WORDMARK_PIX, art::WORDMARK.len() * WORDMARK_PIX)
}

/// rect 集合 → SVG 文本（按颜色分组，省得每个 rect 带一次 fill）。
fn svg(rects: &[Rect], width: usize, height: usize, title: &str) -> String {
    let mut colors: Vec<(String, Vec<&Rect>)> = Vec::new();
    for r in rects {
        match colors.iter_mut().find(|(c, _)| *c == r.4) {
            Some((_, list)) => list.push(r),
            None => colors.push((r.4.clone(), vec![r])),
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width} {height}\" \
         width=\"{}\" height=\"{}\" shape-rendering=\"crispEdges\" role=\"img\" \
         aria-label=\"{title}\">",
        width * SCALE,
        height * SCALE
    );
    let _ = writeln!(out, "<title>{title}</title>");
    for (color, list) in colors {
        let _ = writeln!(out, "<g fill=\"{color}\">");
        for (x, y, w, h, _) in list {
            let _ = writeln!(
                out,
                "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\"/>"
            );
        }
        let _ = writeln!(out, "</g>");
    }
    out.push_str("</svg>\n");
    out
}

/// 横版 lockup：飞行海鸥 + `WING` 大字，两者竖直居中。
fn banner(light: bool) -> (String, usize, usize) {
    let gull = art::FLY_3;
    let gull_w = gull.iter().map(|r| r.chars().count()).max().unwrap_or(0);
    let gull_h = gull.len();
    let (wm, wm_w, wm_h) = wordmark_rects(light, 0, 0);
    let height = gull_h.max(wm_h) + PAD * 2;
    let mut rects = sprite_rects(gull, PAD, (height - gull_h) / 2);
    let wm_x = PAD + gull_w + GAP;
    let wm_y = (height - wm_h) / 2;
    rects.extend(
        wm.into_iter()
            .map(|(x, y, w, h, c)| (x + wm_x, y + wm_y, w, h, c)),
    );
    let width = wm_x + wm_w + PAD;
    (
        svg(
            &rects,
            width,
            height,
            "wing — Towards general agent runtime",
        ),
        width,
        height,
    )
}

fn main() {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "assets".to_string())
        .into();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("export_logo: {e}");
        std::process::exit(1);
    }
    let write = |name: &str, content: String| match fs::write(dir.join(name), content) {
        Ok(()) => println!("wrote {}", dir.join(name).display()),
        Err(e) => {
            eprintln!("export_logo: {e}");
            std::process::exit(1);
        }
    };

    let (dark, _, _) = banner(false);
    write("banner-dark.svg", dark);
    let (light, _, _) = banner(true);
    write("banner-light.svg", light);

    let perched = art::PERCHED_IDLE;
    let w = perched.iter().map(|r| r.chars().count()).max().unwrap_or(0);
    write(
        "gull.svg",
        svg(
            &sprite_rects(perched, 0, 0),
            w,
            perched.len(),
            "wing mascot",
        ),
    );
}
