//! 粘贴图片的压缩：任意支持格式的字节 → 白底 JPEG（目标 ≤ `max_bytes`）。
//!
//! 流程（每一步都只因需求存在；触发通道与 chip 见粘贴通道）：
//!
//! ```text
//! decode          png/jpeg/gif/webp/bmp/tiff（`image` 已开的 codec；GIF 取首帧）
//! → flatten       半透明像素按白底合成（丢 alpha 会变黑边，白底是中性底）
//! → 尺寸上限      长边 > `max_edge` 时一次性缩到 `max_edge`（等比）
//! → JPEG(q)       质量 `quality`
//! → 预算循环      超 `max_bytes` 时先降质（步进 5，下限 60）再降尺寸（长边 ×0.75，下限 256）
//! ```
//!
//! 预算循环的**终止性**来自两个严格单调的量：质量每轮至少降 1（到 60 为止），长边每轮至少缩 1
//! （到 256 为止）；两个都到底仍超预算 → [`CompressError::TooLarge`]，绝不空转。
//!
//! 本模块是 `ui/image/` 之外唯一直接使用 `image` crate 的地方：纯编解码，与终端图形协议无关。

use image::Rgb;
use image::RgbImage;
use image::RgbaImage;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

/// 已知可解码的图片 MIME，按偏好序（png → jpeg → webp → gif → bmp → tiff）。
///
/// 这是上面 codec 清单的 MIME 面孔，两条粘贴通道共用它：OSC 5522 挑终端表示
/// （`tui::clipboard_protocol::choose_mime`）与本地剪贴板的类型选择
/// （`util::clipboard_paste`）。表外的 `image/*`（avif / heic…）一律不选——读回来
/// 也解不了。tiff 在表内：macOS 剪贴板的规范表示（截图）常是 TIFF。
pub(crate) const KNOWN_IMAGE_MIMES: [&str; 6] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/tiff",
];

/// 压缩产物的 MIME：预算循环的产物恒为 JPEG（媒体引用的 `mime` 字段取它）。
pub const JPEG_MIME: &str = "image/jpeg";

/// 压缩产物：JPEG 字节 + 编码时的像素尺寸（mime 恒 `image/jpeg`）。
///
/// `width` / `height` 是**编码后**的尺寸（记录用；调用方不需要自己解码回来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// 压缩参数。
///
/// 来自 `paste.image.*`（[`crate::config::paste::PasteImageConfig::compress_options`] 已把
/// 越界值夹紧）；编码器对 `quality` 再做一次防御性夹紧（1..=100），`max_edge` 取 `max(1)`，
/// 所以手搓的 `CompressOptions` 也不会让循环越界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressOptions {
    /// 目标字节上限（JPEG 输出）。
    pub max_bytes: usize,
    /// JPEG 质量（1..=100）。
    pub quality: u8,
    /// 长边像素上限。
    pub max_edge: u32,
}

/// 质量下限：降质阶梯到此为止，之后只降尺寸。
const QUALITY_FLOOR: u8 = 60;
/// 降质步长。
const QUALITY_STEP: u8 = 5;
/// 尺寸下限（长边像素）：降尺寸阶梯到此为止。
const MIN_LONG_EDGE: u32 = 256;
/// 降尺寸步长（长边比例）。
const SCALE_STEP: f64 = 0.75;

/// 压缩失败面。
#[derive(Debug)]
pub enum CompressError {
    /// 输入不是可解码的图片。
    Decode(image::ImageError),
    /// JPEG 编码器失败（内存 / 内部错误）。
    Encode(image::ImageError),
    /// 到达尺寸下限仍超出 `max_bytes`（预算本身不可达）。
    TooLarge {
        /// 最小一次尝试的字节数。
        bytes: usize,
        /// 目标上限。
        max_bytes: usize,
    },
}

impl std::fmt::Display for CompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "cannot decode the image: {e}"),
            Self::Encode(e) => write!(f, "JPEG encoding failed: {e}"),
            Self::TooLarge { bytes, max_bytes } => write!(
                f,
                "cannot compress below {max_bytes} bytes (smallest attempt: {bytes} bytes)"
            ),
        }
    }
}

impl std::error::Error for CompressError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(e) | Self::Encode(e) => Some(e),
            Self::TooLarge { .. } => None,
        }
    }
}

/// 压缩一张图片：解码 → 白底 flatten → 长边 ≤ `max_edge` → JPEG(q) → 预算循环。
pub fn compress_image(
    input: &[u8],
    opts: &CompressOptions,
) -> Result<CompressedImage, CompressError> {
    let decoded = image::load_from_memory(input).map_err(CompressError::Decode)?;
    let mut rgb = flatten_over_white(&decoded);
    let max_edge = opts.max_edge.max(1);
    if rgb.width().max(rgb.height()) > max_edge {
        rgb = scale_to_long_edge(&rgb, max_edge);
    }
    encode_within_budget(rgb, opts)
}

/// RGBA → RGB：半透明像素按白底合成，`c' = (c·a + 255·(255−a)) / 255`。
///
/// 不透明输入是恒等变换（a = 255），所以 RGB / 灰度来源不因这步改变像素。
fn flatten_over_white(image: &image::DynamicImage) -> RgbImage {
    let rgba: RgbaImage = image.to_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());
    for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
        let alpha = u32::from(src[3]);
        let blend = |c: u8| ((u32::from(c) * alpha + 255 * (255 - alpha)) / 255) as u8;
        *dst = Rgb([blend(src[0]), blend(src[1]), blend(src[2])]);
    }
    rgb
}

/// 等比缩到「长边 == `target`」（短边四舍五入、至少 1 像素）。
fn scale_to_long_edge(image: &RgbImage, target: u32) -> RgbImage {
    let (width, height) = image.dimensions();
    let (nw, nh) = scaled_dims(width, height, target);
    image::imageops::resize(image, nw, nh, FilterType::Lanczos3)
}

/// 长边 == `target` 的等比尺寸（短边 ≥ 1）。
fn scaled_dims(width: u32, height: u32, target: u32) -> (u32, u32) {
    debug_assert!(width > 0 && height > 0);
    if width >= height {
        let short = (f64::from(height) * f64::from(target) / f64::from(width)).round() as u32;
        (target, short.max(1))
    } else {
        let short = (f64::from(width) * f64::from(target) / f64::from(height)).round() as u32;
        (short.max(1), target)
    }
}

/// 预算循环：先降质、再降尺寸，两阶梯都到底仍超限 → [`CompressError::TooLarge`]。
fn encode_within_budget(
    mut image: RgbImage,
    opts: &CompressOptions,
) -> Result<CompressedImage, CompressError> {
    let mut quality = opts.quality.clamp(1, 100);
    loop {
        let bytes = encode_jpeg(&image, quality)?;
        if bytes.len() <= opts.max_bytes {
            return Ok(CompressedImage {
                bytes,
                width: image.width(),
                height: image.height(),
            });
        }
        if quality > QUALITY_FLOOR {
            quality = quality.saturating_sub(QUALITY_STEP).max(QUALITY_FLOOR);
            continue;
        }
        let long_edge = image.width().max(image.height());
        if long_edge <= MIN_LONG_EDGE {
            return Err(CompressError::TooLarge {
                bytes: bytes.len(),
                max_bytes: opts.max_bytes,
            });
        }
        // 严格小于当前长边（`min(long_edge - 1)`）：终止性不依赖两个常量之间的大小关系。
        let stepped = (f64::from(long_edge) * SCALE_STEP).round() as u32;
        let target = stepped.max(MIN_LONG_EDGE).min(long_edge - 1);
        image = scale_to_long_edge(&image, target);
    }
}

fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>, CompressError> {
    let mut bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
    encoder.encode_image(image).map_err(CompressError::Encode)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::DynamicImage;
    use image::ImageFormat;
    use image::Rgba;

    const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

    fn defaults() -> CompressOptions {
        CompressOptions {
            max_bytes: 1024 * 1024,
            quality: 85,
            max_edge: 2048,
        }
    }

    /// 确定性噪声（无 rand 依赖）：LRU 压缩不掉的无结构图，用来逼降质 / 降尺寸阶梯。
    fn noise(width: u32, height: u32, seed: u64) -> RgbImage {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        RgbImage::from_fn(width, height, |_, _| {
            let v = next();
            Rgb([v as u8, (v >> 8) as u8, (v >> 16) as u8])
        })
    }

    fn png_bytes(image: DynamicImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode png fixture");
        bytes
    }

    fn decode(bytes: &[u8]) -> DynamicImage {
        image::load_from_memory(bytes).expect("decode compressed output")
    }

    fn center_pixel(image: &DynamicImage) -> Rgb<u8> {
        image
            .to_rgb8()
            .get_pixel(image.width() / 2, image.height() / 2)
            .to_owned()
    }

    #[test]
    fn a_normal_image_keeps_its_geometry() {
        let source = noise(800, 600, 0xA11CE);
        let out = compress_image(&png_bytes(DynamicImage::ImageRgb8(source)), &defaults())
            .expect("compress");
        assert_eq!((out.width, out.height), (800, 600));
        assert!(out.bytes.len() <= defaults().max_bytes);
        // JPEG 魔数（SOI + APP0）：上传的确实是 JPEG，不是别的容器。
        assert_eq!(&out.bytes[..3], &[0xFF, 0xD8, 0xFF]);
        assert_eq!(decode(&out.bytes).to_rgb8().dimensions(), (800, 600));
    }

    #[test]
    fn an_oversized_long_edge_is_scaled_to_the_limit() {
        let opts = CompressOptions {
            max_edge: 1000,
            ..defaults()
        };
        let landscape = compress_image(
            &png_bytes(DynamicImage::ImageRgb8(noise(3000, 1000, 1))),
            &opts,
        )
        .expect("compress");
        assert_eq!((landscape.width, landscape.height), (1000, 333));

        let portrait = compress_image(
            &png_bytes(DynamicImage::ImageRgb8(noise(500, 4000, 2))),
            &opts,
        )
        .expect("compress");
        assert_eq!((portrait.width, portrait.height), (125, 1000));
    }

    /// 质量阶梯能独立达标：几何不变、只降质（噪声图的质量 ↔ 尺寸曲线是真的有斜率）。
    #[test]
    fn the_quality_ladder_alone_can_meet_the_budget() {
        let png = png_bytes(DynamicImage::ImageRgb8(noise(256, 256, 0xD00D)));
        // 质量下限处的产物尺寸（预算不设限）：预算定在它之上一点 ⇒ 只有降质阶梯能达标。
        let floor_size = compress_image(
            &png,
            &CompressOptions {
                max_bytes: usize::MAX,
                quality: QUALITY_FLOOR,
                ..defaults()
            },
        )
        .expect("compress")
        .bytes
        .len();
        let full_quality = compress_image(&png, &defaults())
            .expect("compress")
            .bytes
            .len();
        assert!(
            full_quality > floor_size,
            "样本要足够大：{full_quality} > {floor_size}"
        );

        let opts = CompressOptions {
            max_bytes: floor_size + 64,
            ..defaults()
        };
        let out = compress_image(&png, &opts).expect("compress");
        assert!(out.bytes.len() <= opts.max_bytes);
        assert_eq!((out.width, out.height), (256, 256), "只降质，不缩尺寸");
    }

    /// 预算在质量下限之外：进入降尺寸阶梯，长边变小且最终达标。
    #[test]
    fn the_size_ladder_shrinks_until_the_budget_fits() {
        let opts = CompressOptions {
            max_bytes: 32 * 1024,
            ..defaults()
        };
        let out = compress_image(
            &png_bytes(DynamicImage::ImageRgb8(noise(1024, 1024, 0xBEEF))),
            &opts,
        )
        .expect("compress");
        assert!(out.bytes.len() <= opts.max_bytes);
        assert!(out.width.max(out.height) < 1024, "必须真的缩过尺寸");
        assert!(out.width.max(out.height) >= MIN_LONG_EDGE);
        assert_eq!(out.width, out.height, "等比缩放不改变宽高比");
    }

    /// 不可达的预算：到尺寸下限后如实报错（而不是空转或超限发出去）。
    #[test]
    fn an_unreachable_budget_errors_at_the_size_floor() {
        let opts = CompressOptions {
            max_bytes: 16,
            ..defaults()
        };
        let err = compress_image(
            &png_bytes(DynamicImage::ImageRgb8(noise(512, 512, 7))),
            &opts,
        )
        .expect_err("16 bytes is not reachable");
        match err {
            CompressError::TooLarge { bytes, max_bytes } => {
                assert_eq!(max_bytes, 16);
                assert!(bytes > 16);
            }
            other => panic!("expected TooLarge, got {other}"),
        }
        assert!(err.to_string().contains("16"), "{err}");
    }

    /// 全透明像素压到白底（丢 alpha 变黑边是这条测试挡住的回归）。
    #[test]
    fn transparent_pixels_flatten_onto_white() {
        let rgba = RgbaImage::from_pixel(16, 16, Rgba([255, 0, 0, 0]));
        let out = compress_image(&png_bytes(DynamicImage::ImageRgba8(rgba)), &defaults())
            .expect("compress");
        let pixel = center_pixel(&decode(&out.bytes));
        for (channel, value) in pixel.0.iter().enumerate() {
            assert!(
                *value >= 250,
                "全透明 = 白底，通道 {channel} 实际 {value}（pixel={pixel:?}）"
            );
        }
    }

    /// 半透明像素按权重合成：50% 红 → 粉（不是纯红、也不是纯白）。
    #[test]
    fn semi_transparent_pixels_blend_with_the_weight() {
        let rgba = RgbaImage::from_pixel(16, 16, Rgba([255, 0, 0, 128]));
        let out = compress_image(&png_bytes(DynamicImage::ImageRgba8(rgba)), &defaults())
            .expect("compress");
        let pixel = center_pixel(&decode(&out.bytes));
        let [r, g, b] = pixel.0;
        assert!(r >= 245, "红通道该留在顶部：{pixel:?}");
        assert!(
            (120..=140).contains(&g) && (120..=140).contains(&b),
            "绿蓝 ≈ 127（白底的一半）：{pixel:?}"
        );
    }

    /// 不透明 RGB 输入不被 flatten 改动（恒等变换）。
    #[test]
    fn opaque_pixels_are_left_alone() {
        let rgb = RgbImage::from_pixel(16, 16, Rgb([255, 0, 0]));
        let out = compress_image(&png_bytes(DynamicImage::ImageRgb8(rgb)), &defaults())
            .expect("compress");
        let [r, g, b] = center_pixel(&decode(&out.bytes)).0;
        assert!(r >= 245, "红：{r}");
        assert!(g <= 10 && b <= 10, "不该混入白底：{g},{b}");
    }

    /// GIF 取首帧：两帧（红 → 蓝）的动图压出来是红，不是蓝、也不是拼接。
    #[test]
    fn an_animated_gif_uses_the_first_frame() {
        let red = RgbaImage::from_pixel(16, 16, Rgba([255, 0, 0, 255]));
        let blue = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 255, 255]));
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            encoder
                .encode_frame(image::Frame::new(red))
                .expect("frame 1");
            encoder
                .encode_frame(image::Frame::new(blue))
                .expect("frame 2");
        }
        let out = compress_image(&gif, &defaults()).expect("compress");
        let [r, g, b] = center_pixel(&decode(&out.bytes)).0;
        assert!(r >= 245 && g <= 10 && b <= 10, "首帧是红：{r},{g},{b}");
    }

    /// macOS 剪贴板的规范表示常是 TIFF（截图）：tiff codec 是本地读取与 OSC 5522
    /// 两条粘贴通道的共同前提，压缩走与其它格式相同的 decode → flatten → JPEG 路径。
    #[test]
    fn a_tiff_source_compresses_like_any_other_picture() {
        let rgba = RgbaImage::from_pixel(20, 10, Rgba([255, 0, 0, 128]));
        let mut tiff = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut tiff), ImageFormat::Tiff)
            .expect("encode tiff fixture");
        let out = compress_image(&tiff, &defaults()).expect("compress tiff");
        assert_eq!(&out.bytes[..3], &[0xFF, 0xD8, 0xFF], "产物是 JPEG");
        assert_eq!((out.width, out.height), (20, 10));
        // 半透明像素按白底合成（flatten 对 TIFF 同样生效）。
        let [r, g, b] = center_pixel(&decode(&out.bytes)).0;
        assert!(r >= 245, "红通道留在顶部：{r}");
        assert!(
            (120..=140).contains(&g) && (120..=140).contains(&b),
            "绿蓝 ≈ 127（白底的一半）：{g},{b}"
        );
    }

    /// `KNOWN_IMAGE_MIMES` 的每一项都必须真有编解码器：它是两条粘贴通道的选型依据，
    /// 表里混进一个没开的 codec，用户会收到一张「读得回来但解不了」的图。
    #[test]
    fn every_known_mime_has_a_compiled_in_codec() {
        for mime in KNOWN_IMAGE_MIMES {
            let format = ImageFormat::from_mime_type(mime)
                .unwrap_or_else(|| panic!("{mime} is not an `image` format"));
            // 真编一份再解一份：feature 没开时 from_mime_type 仍认得格式，
            // 只有缺 codec 才会在这一步炸出来。
            let mut encoded = Vec::new();
            DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, WHITE))
                .write_to(&mut std::io::Cursor::new(&mut encoded), format)
                .unwrap_or_else(|e| panic!("cannot encode a {mime} fixture: {e}"));
            image::load_from_memory(&encoded)
                .unwrap_or_else(|e| panic!("{mime} has no compiled-in codec: {e}"));
        }
    }

    #[test]
    fn garbage_bytes_are_a_decode_error_not_a_panic() {
        let err = compress_image(b"this is not an image", &defaults()).expect_err("not an image");
        assert!(matches!(err, CompressError::Decode(_)), "{err:?}");
        assert!(err.to_string().contains("decode"), "{err}");
    }

    #[test]
    fn a_one_pixel_image_is_legal() {
        let out = compress_image(
            &png_bytes(DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, WHITE))),
            &defaults(),
        )
        .expect("compress");
        assert_eq!((out.width, out.height), (1, 1));
        assert!(out.bytes.len() < 1024);
    }

    /// 小图（低于尺寸下限）配不可达预算：立即报错，不进入降尺寸阶梯。
    #[test]
    fn an_image_below_the_size_floor_skips_the_size_ladder() {
        let opts = CompressOptions {
            max_bytes: 16,
            ..defaults()
        };
        let err = compress_image(&png_bytes(DynamicImage::ImageRgb8(noise(64, 64, 3))), &opts)
            .expect_err("too small to shrink further");
        assert!(matches!(err, CompressError::TooLarge { .. }), "{err:?}");
    }

    /// `quality` 的夹紧在 config 侧；压缩器对 1 / 100 两个端点必须都工作。
    #[test]
    fn the_quality_endpoints_encode() {
        for quality in [1u8, 100u8] {
            let opts = CompressOptions {
                quality,
                ..defaults()
            };
            let out = compress_image(&png_bytes(DynamicImage::ImageRgb8(noise(32, 32, 5))), &opts)
                .unwrap_or_else(|e| panic!("quality {quality}: {e}"));
            assert_eq!(decode(&out.bytes).to_rgb8().dimensions(), (32, 32));
        }
    }
}
