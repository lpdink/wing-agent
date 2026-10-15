//! OSC 5522 粘贴通道的 App 边界（`app/clipboard.rs`）。
//!
//! 断言的是**接线语义**：文本与 bracketed paste 同一条路（穿过模态挡板），图片走
//! 「入队 → 后台压缩 → 图片 chip」的单并发流水线（压缩真跑一遍，不假装），失败只弹
//! toast 不动内容。

use super::support::*;
use crate::app::App;
use crate::app::AppIntent;
use crate::app::clipboard::MAX_PENDING_IMAGE_BYTES;
use crate::app::clipboard::MAX_PENDING_IMAGES;
use crate::tui::ClipboardFailure;
use crate::tui::ClipboardPayload;
use crate::ui::toast::ToastKind;
use crate::util::clipboard_paste::PasteFailure;
use crate::util::clipboard_paste::PasteOutcome;
use crate::util::image_compress::CompressOptions;
use crate::util::image_compress::CompressedImage;
use crate::util::image_compress::compress_image;

/// 一张真的 PNG（`image` crate 编码）：压缩链路要真跑一遍，不能在测试里假装。
fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let buffer = image::RgbImage::from_pixel(width, height, image::Rgb(rgb));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(buffer)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode the fixture PNG");
    bytes.into_inner()
}

/// 一张压缩产物在草稿里应当显示的 chip 文案。
///
/// 这里只覆盖 `B` 档（测试的合成图都远小于 1 KiB）；KB / MB 两档的格式由
/// `ui::input_area::image` 的单测钉死，本文件关心的是**接线**：chip 里的数字必须来自
/// 真正要发出去的那份字节。
fn chip_text(number: usize, image: &CompressedImage) -> String {
    format!(
        "[Image #{number} {}×{} {}B]",
        image.width,
        image.height,
        image.bytes.len()
    )
}

/// 跑一遍真压缩。
fn compress(bytes: &[u8], options: &CompressOptions) -> Result<CompressedImage, String> {
    compress_image(bytes, options).map_err(|e| e.to_string())
}

/// 取出一条压缩 intent（出队即消费：字节与选项都在里面）。
fn take_compression(app: &mut App) -> (Vec<u8>, CompressOptions) {
    let intents = app.drain_intents();
    let Some(AppIntent::CompressClipboardImage { bytes, options }) = intents
        .into_iter()
        .find(|i| matches!(i, AppIntent::CompressClipboardImage { .. }))
    else {
        panic!("粘贴必须推出一条压缩 intent");
    };
    (bytes, options)
}

/// 入队一张图（测试的合成图或任意字节）。
fn paste_image(app: &mut App, bytes: Vec<u8>, name: Option<&str>) {
    app.handle_clipboard(Ok(ClipboardPayload::Image {
        bytes,
        mime: "image/png".into(),
        name: name.map(str::to_string),
    }));
}

#[test]
fn text_payload_lands_in_the_composer() {
    let mut app = test_app();
    app.handle_clipboard(Ok(ClipboardPayload::Text("hello 图片".into())));
    assert_eq!(app.input.text(), "hello 图片");
}

/// 文本走的就是 `handle_paste`：模态挡板的语义自动一致（这里用「模态选择面板开着」
/// 这条最显眼的挡板），不需要通道各自实现一遍。
#[test]
fn text_payload_respects_the_composer_gate() {
    let mut app = test_app();
    app.model_panel = Some(crate::shared::panels::picker::ModelPanel::new(
        Vec::new(),
        None,
    ));
    app.handle_clipboard(Ok(ClipboardPayload::Text("hidden".into())));
    assert_eq!(app.input.text(), "", "面板开着时通道文本也不进草稿");
}

#[test]
fn failure_only_toasts() {
    let mut app = test_app();
    app.handle_clipboard(Err(ClipboardFailure::Denied));
    assert!(app.pending_clipboard_images().is_empty());
    assert_eq!(app.input.text(), "");
    let toast = app.toast.as_ref().expect("失败要可见");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert!(
        toast.message.contains("denied"),
        "文案来自 ClipboardFailure::message：{toast:?}"
    );
}

/// 通道失败与 bracketed paste 并存：失败不动任何用户内容，随后的正常粘贴照旧。
#[test]
fn failure_does_not_break_later_pastes() {
    let mut app = test_app();
    app.handle_clipboard(Err(ClipboardFailure::Timeout));
    app.handle_clipboard(Ok(ClipboardPayload::Text("still works".into())));
    assert_eq!(app.input.text(), "still works");
}

/// 协议机队列满（连续粘贴超出上限）的丢弃同样落成 toast：丢弃可以，静默不行
/// （红线：不出现「粘了但什么都没发生」）。
#[test]
fn a_full_paste_queue_reaches_the_toast() {
    let mut app = test_app();
    app.handle_clipboard(Err(ClipboardFailure::QueueFull));
    let toast = app.toast.as_ref().expect("丢弃要看得见");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert_eq!(toast.message, "Paste dropped: the paste queue is full");
}

/// 一张合成图走完整条链路：Paste event → 队列 → intent → 真压缩 → chip。
#[test]
fn a_pasted_picture_becomes_an_image_chip() {
    let mut app = test_app();
    let original = png(64, 32, [200, 30, 30]);
    paste_image(&mut app, original.clone(), None);

    // 出队即消费：队列里不再有它，字节与选项在 intent 里。
    assert!(
        app.pending_clipboard_images().is_empty(),
        "队首已经出队去压了"
    );
    let (bytes, options) = take_compression(&mut app);
    assert_eq!(options, app.config.paste.image.compress_options());
    assert_eq!(bytes, original, "intent 带的是原始字节");

    // 压缩是真的：产物是 JPEG、尺寸没被改（64×32 < max_edge）。
    let image = compress(&bytes, &options).expect("compress");
    assert_eq!(image.bytes[..2], [0xFF, 0xD8], "JPEG SOI");
    assert_eq!((image.width, image.height), (64, 32));
    let label = chip_text(1, &image);
    app.on_clipboard_compressed(Ok(image));

    assert_eq!(app.input.text(), label, "chip 出现在草稿里，标签是契约文案");
    assert_eq!(
        (app.input.cursor_row, app.input.cursor_col),
        (0, label.chars().count()),
        "光标落在 chip 之后"
    );
    assert!(app.toast.is_none(), "成功不 toast：chip 就是反馈");
    assert!(
        !app.composer_typing_blocked(),
        "图片粘贴不改键盘归属（草稿照常可编辑）"
    );
}

/// 队列是 FIFO：两次粘贴按顺序压、按顺序插 —— chip 号码 = 粘贴顺序。
#[test]
fn two_pastes_keep_their_order() {
    let mut app = test_app();
    paste_image(&mut app, png(8, 8, [1, 2, 3]), None);
    paste_image(&mut app, png(16, 16, [4, 5, 6]), Some("second.png"));
    assert_eq!(
        app.pending_clipboard_images().len(),
        1,
        "第二张等着第一张压完（单并发）"
    );

    let (bytes, options) = take_compression(&mut app);
    assert!(
        app.drain_intents().is_empty(),
        "在飞期间只有一条 intent（第二张还没出队）"
    );
    let first = compress(&bytes, &options).expect("first");
    app.on_clipboard_compressed(Ok(first.clone()));

    // 第一张落地的同时泵出第二张。
    let (bytes, options) = take_compression(&mut app);
    assert_eq!(app.pending_clipboard_images().len(), 0, "第二张也出队了");
    let second = compress(&bytes, &options).expect("second");
    app.on_clipboard_compressed(Ok(second.clone()));

    assert_eq!(
        app.input.text(),
        format!("{}{}", chip_text(1, &first), chip_text(2, &second)),
        "号码随粘贴顺序，不是随压缩完成的顺序"
    );
    assert!(app.drain_intents().is_empty(), "队列空了就不再泵");
}

/// 压缩失败：toast 说明原因，草稿不动，流水线继续（下一张照压）。
#[test]
fn a_picture_that_cannot_be_compressed_toasts_and_the_pipeline_goes_on() {
    let mut app = test_app();
    paste_image(&mut app, vec![0x00, 0x01, 0x02, 0x03], None);
    paste_image(&mut app, png(8, 8, [9, 9, 9]), None);
    let _ = take_compression(&mut app);

    // 第一张不是图片：压缩器回 Err（真跑一遍，不假装）。
    let options = app.config.paste.image.compress_options();
    let failed =
        compress(&[0x00, 0x01, 0x02, 0x03], &options).expect_err("garbage must not decode");
    app.on_clipboard_compressed(Err(failed));

    let toast = app.toast.as_ref().expect("失败要可见");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert!(
        toast.message.starts_with("Image dropped:"),
        "文案要说清丢的是什么：{toast:?}"
    );
    assert_eq!(app.input.text(), "", "失败的图不留痕");

    // 第二张照旧出队 → 压 → 插 chip。
    let (bytes, options) = take_compression(&mut app);
    let image = compress(&bytes, &options).expect("second");
    app.on_clipboard_compressed(Ok(image.clone()));
    assert_eq!(app.input.text(), chip_text(1, &image));
}

/// 会话切换不该丢掉压缩回执：草稿还在，chip 就还得插（`handle_fetch_result` 的守卫
/// 对这条回执不生效）。丢掉它流水线会永远停在「在飞」，之后所有粘贴都不落地。
#[test]
fn a_compression_result_survives_a_session_switch() {
    let mut app = test_app();
    let bytes = png(8, 8, [3, 3, 3]);
    let options = app.config.paste.image.compress_options();
    paste_image(&mut app, bytes, None);
    let (queued, _) = take_compression(&mut app);

    // 会话换了（压缩还在飞）。
    app.session_id = "another-session".into();
    let compressed = compress(&queued, &options).expect("compress");
    app.handle_fetch_result(crate::app::intent::FetchResult {
        session_id: "test-session".into(),
        payload: crate::app::intent::FetchPayload::ClipboardImage(Ok(compressed.clone())),
    });

    assert_eq!(
        app.input.text(),
        chip_text(1, &compressed),
        "回执按 composer 的语义落地，不按会话丢弃"
    );
}

/// 图片粘贴不受模态挡板约束：文本是往某个输入框送字，模态面板该拦；图片落的是草稿上
/// 一个可见、可删的 chip，静默丢字节才是最坏的结果。
#[test]
fn a_picture_is_not_gated_by_a_modal_panel() {
    let mut app = test_app();
    app.model_panel = Some(crate::shared::panels::picker::ModelPanel::new(
        Vec::new(),
        None,
    ));
    paste_image(&mut app, vec![1, 2, 3], None);
    assert!(
        app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::CompressClipboardImage { .. })),
        "字节进了流水线，不是被面板吃掉"
    );
}

/// 队列上限（条数）：到顶丢最旧的一张，用户刚粘的那张永远留着；**丢弃要出声**
/// （红线：不出现「粘了但什么都没发生」——这里与协议机队列满走同一条反馈）。
#[test]
fn the_queue_is_capped_by_count() {
    let mut app = test_app();
    // 只填不泵：把「在飞」占住，后面的全留在队列里。
    app.clipboard_compress_in_flight = true;
    for i in 0..(MAX_PENDING_IMAGES + 3) {
        paste_image(&mut app, vec![i as u8; 4], None);
    }
    let queued = app.pending_clipboard_images();
    assert_eq!(queued.len(), MAX_PENDING_IMAGES);
    assert_eq!(queued[0].bytes[0], 3, "丢的是最旧的那张（FIFO）");
    assert_eq!(
        queued[queued.len() - 1].bytes[0],
        (MAX_PENDING_IMAGES + 2) as u8,
        "最新的一张必须留着"
    );
    let toast = app.toast.as_ref().expect("丢图要看得见");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert_eq!(toast.message, "Image dropped: too many pending images");
}

/// 队列上限（总字节）：一次 64 MiB 级的大图不会把内存顶穿；丢弃同样有 toast。
#[test]
fn the_queue_is_capped_by_bytes() {
    let mut app = test_app();
    app.clipboard_compress_in_flight = true;
    let bite = 24 * 1024 * 1024; // 24 MiB × 4 张 > 64 MiB 上限
    for i in 0..4u8 {
        paste_image(&mut app, vec![i; bite], None);
    }
    let total: usize = app
        .pending_clipboard_images()
        .iter()
        .map(|i| i.bytes.len())
        .sum();
    assert!(
        total <= MAX_PENDING_IMAGE_BYTES,
        "总字节必须回到预算内：{total}"
    );
    assert_eq!(
        app.pending_clipboard_images().last().map(|i| i.bytes[0]),
        Some(3),
        "最新的一张必须留着"
    );
    let toast = app.toast.as_ref().expect("丢图要看得见");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert_eq!(toast.message, "Image dropped: too many pending images");
}

/// 多张图累积进同一份草稿，与文本粘贴混排：chip 号码各自单调，提交产物两路分开。
#[test]
fn pictures_and_text_share_the_draft() {
    let mut app = test_app();
    app.handle_clipboard(Ok(ClipboardPayload::Text("see this ".into())));
    paste_image(&mut app, png(8, 8, [1, 1, 1]), None);
    let (bytes, options) = take_compression(&mut app);
    let image = compress(&bytes, &options).expect("compress");
    app.on_clipboard_compressed(Ok(image.clone()));
    app.handle_clipboard(Ok(ClipboardPayload::Text("and this".into())));

    assert_eq!(
        app.input.text(),
        format!("see this{}and this", chip_text(1, &image)),
        "chip 内联在光标处，后面的文本留在同一行"
    );
}

/// runner 的阻塞池那一跳：`compress_clipboard_image` 在真 tokio 运行时里把一张 PNG
/// 变成 JPEG 产物，错误面转成给 toast 的一句话。
///
/// 直接驱动抽出来的函数而不是 `execute_intent`：后者需要 `WingTerminal`（真实
/// stdout），单测造不出来（同 `deliver_user_message` 的抽取理由）。
#[tokio::test]
async fn the_runner_compresses_on_the_blocking_pool() {
    let mut app = test_app();
    paste_image(&mut app, png(48, 24, [10, 200, 10]), None);
    let (bytes, options) = take_compression(&mut app);

    let image = super::super::runner::compress_clipboard_image(bytes, options)
        .await
        .expect("compress");
    assert_eq!(image.bytes[..2], [0xFF, 0xD8], "JPEG SOI");
    assert_eq!((image.width, image.height), (48, 24));
    app.on_clipboard_compressed(Ok(image));
    let text = app.input.text();
    assert!(
        text.starts_with("[Image #1 48×24 ") && text.ends_with(']'),
        "草稿里的 chip 文案：{text}"
    );

    let error = super::super::runner::compress_clipboard_image(vec![1, 2, 3], options)
        .await
        .expect_err("garbage must not decode");
    assert!(error.contains("cannot decode"), "错误文案给 toast：{error}");
}

// ── L1（本地读取）的回执落点 ─────────────────────────────────────────

/// 本地读取的文本与 bracketed paste / OSC 5522 通道**同一个入口**（`handle_paste`），
/// 模态挡板的语义自动一致。
#[test]
fn local_paste_text_lands_in_the_composer() {
    let mut app = test_app();
    app.on_clipboard_paste(Ok(PasteOutcome::Text("local 文本".into())));
    assert_eq!(app.input.text(), "local 文本");
}

#[test]
fn local_paste_text_respects_the_composer_gate() {
    let mut app = test_app();
    app.model_panel = Some(crate::shared::panels::picker::ModelPanel::new(
        Vec::new(),
        None,
    ));
    app.on_clipboard_paste(Ok(PasteOutcome::Text("hidden".into())));
    assert_eq!(app.input.text(), "", "面板开着时本地读取的文本也不进草稿");
}

/// 本地读取的图片进既有单并发流水线（与 OSC 5522 图片同一落点，同样不受挡板）。
#[test]
fn local_paste_image_joins_the_pipeline() {
    let mut app = test_app();
    app.on_clipboard_paste(Ok(PasteOutcome::Image {
        bytes: vec![1, 2, 3],
        mime: "image/png".into(),
    }));
    assert!(
        app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::CompressClipboardImage { .. })),
        "字节进了压缩流水线"
    );
    assert_eq!(app.pending_clipboard_images().len(), 0, "队首已出队去压");
}

/// 空剪贴板 / 失败都要出声（红线：不静默），且不动用户已有内容。
#[test]
fn local_paste_empty_and_failure_speak_up() {
    let mut app = test_app();
    app.input.set_text("draft stays");
    app.on_clipboard_paste(Ok(PasteOutcome::Empty));
    let toast = app.toast.as_ref().expect("空剪贴板要看得见");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert_eq!(toast.message, "Clipboard has no text or image");
    assert_eq!(app.input.text(), "draft stays", "失败面不碰草稿");

    for failure in [
        PasteFailure::Remote,
        PasteFailure::Unavailable,
        PasteFailure::Timeout,
    ] {
        let mut app = test_app();
        app.on_clipboard_paste(Err(failure.clone()));
        let toast = app.toast.as_ref().expect("失败要看得见");
        assert_eq!(toast.kind, ToastKind::Warning);
        assert_eq!(toast.message, failure.message(), "定向文案：{failure:?}");
        assert_eq!(app.input.text(), "");
    }
}

/// 回执与压缩回执同一条契约：不属于任何会话（换过会话草稿还在）——
/// `handle_fetch_result` 在会话守卫**之前**分派它。
#[test]
fn local_paste_result_survives_a_session_switch() {
    let mut app = test_app();
    app.session_id = "another-session".into();
    app.handle_fetch_result(crate::app::intent::FetchResult {
        session_id: "test-session".into(),
        payload: crate::app::intent::FetchPayload::ClipboardPaste(Ok(PasteOutcome::Text(
            "kept".into(),
        ))),
    });
    assert_eq!(app.input.text(), "kept");
}
