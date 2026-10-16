//! OSC 5522 粘贴通道的 App 边界（`app/clipboard.rs`）。
//!
//! 断言的是**接线语义**：文本与 bracketed paste 同一条路（穿过模态挡板），图片走
//! 「占位符 chip → 字节 → 后台压缩 → 原地落定」的单并发流水线（压缩真跑一遍，不假装），
//! 失败只弹 toast 并原地翻成 `✗`。

use super::support::*;
use crate::app::App;
use crate::app::AppIntent;
use crate::app::clipboard::MAX_PENDING_IMAGE_BYTES;
use crate::app::clipboard::MAX_PENDING_IMAGES;
use crate::tui::ClipboardEvent;
use crate::tui::ClipboardFailure;
use crate::tui::ClipboardPayload;
use crate::ui::input_area::ImageChipTarget;
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

/// 取出一条压缩 intent（出队即消费：字节、选项与占位符句柄都在里面）。
fn take_compression(app: &mut App) -> (ImageChipTarget, Vec<u8>, CompressOptions) {
    let intents = app.drain_intents();
    let Some(AppIntent::CompressClipboardImage {
        target,
        bytes,
        options,
    }) = intents
        .into_iter()
        .find(|i| matches!(i, AppIntent::CompressClipboardImage { .. }))
    else {
        panic!("粘贴必须推出一条压缩 intent");
    };
    (target, bytes, options)
}

/// 压缩完并把回执喂回 App（生产路径是后台任务 → `handle_fetch_result`）。
fn settle(
    app: &mut App,
    target: ImageChipTarget,
    bytes: &[u8],
    options: &CompressOptions,
) -> CompressedImage {
    let image = compress(bytes, options).expect("compress");
    app.on_clipboard_compressed(target, Ok(image.clone()));
    image
}

/// 5522 通道的一次图片读开始（占位符出现的时刻）；返回刚建的那个在途句柄
/// （队尾 —— 队首是更早的那条读）。
fn start_image_read(app: &mut App) -> ImageChipTarget {
    app.handle_clipboard(ClipboardEvent::ImageReadStarted {
        mime: "image/png".into(),
    });
    *app.image_reads.back().expect("在途目标")
}

/// 5522 通道的一次图片读交付（字节到手，进压缩流水线）。
fn deliver_image(app: &mut App, bytes: Vec<u8>, name: Option<&str>) {
    app.handle_clipboard(ClipboardEvent::Delivered(ClipboardPayload::Image {
        bytes,
        mime: "image/png".into(),
        name: name.map(str::to_string),
    }));
}

/// 入队一张图（本地通道的形态：读回字节才知道是图片，占位符此刻插）。
fn paste_image(app: &mut App, bytes: Vec<u8>) {
    app.on_clipboard_paste(Ok(PasteOutcome::Image {
        bytes,
        mime: "image/png".into(),
    }));
}

#[test]
fn text_payload_lands_in_the_composer() {
    let mut app = test_app();
    app.handle_clipboard(ClipboardEvent::Delivered(ClipboardPayload::Text(
        "hello 图片".into(),
    )));
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
    app.handle_clipboard(ClipboardEvent::Delivered(ClipboardPayload::Text(
        "hidden".into(),
    )));
    assert_eq!(app.input.text(), "", "面板开着时通道文本也不进草稿");
}

#[test]
fn failure_only_toasts() {
    let mut app = test_app();
    app.handle_clipboard(ClipboardEvent::ReadFailed {
        mime: "image/png".into(),
        failure: ClipboardFailure::Denied,
    });
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
    app.handle_clipboard(ClipboardEvent::ReadFailed {
        mime: "text/plain".into(),
        failure: ClipboardFailure::Timeout,
    });
    app.handle_clipboard(ClipboardEvent::Delivered(ClipboardPayload::Text(
        "still works".into(),
    )));
    assert_eq!(app.input.text(), "still works");
}

/// 协议机队列满（连续粘贴超出上限）的丢弃同样落成 toast：丢弃可以，静默不行
/// （红线：不出现「粘了但什么都没发生」）。
#[test]
fn a_full_paste_queue_reaches_the_toast() {
    let mut app = test_app();
    app.handle_clipboard(ClipboardEvent::PasteRejected(ClipboardFailure::QueueFull));
    let toast = app.toast.as_ref().expect("丢弃要看得见");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert_eq!(toast.message, "Paste dropped: the paste queue is full");
}

/// [门禁语义] 被丢弃的是**另一次**粘贴：正在传的那张图的占位符不许被它误伤
/// （QueueFull 与在途读无关联，翻成 `✗` 会让一张好图凭空失败）。
#[test]
fn a_rejected_paste_leaves_the_in_flight_placeholder_alone() {
    let mut app = test_app();
    let target = start_image_read(&mut app);
    app.handle_clipboard(ClipboardEvent::PasteRejected(ClipboardFailure::QueueFull));
    assert_eq!(app.input.text(), "[Image #1 ⠋]", "占位符照旧");
    assert_eq!(app.image_reads.front(), Some(&target), "在途目标还在");

    // 那条读照常收：进度、交付、落定都不受影响。
    app.handle_clipboard(ClipboardEvent::ReadProgress { received: 4096 });
    assert_eq!(app.input.text(), "[Image #1 ⠋ ↓4KB]");
    deliver_image(&mut app, png(8, 8, [1, 2, 3]), None);
    let (target, bytes, options) = take_compression(&mut app);
    let image = settle(&mut app, target, &bytes, &options);
    assert_eq!(app.input.text(), chip_text(1, &image));
}

// ── 占位符：粘贴那一刻就看得见 ────────────────────────────────────────

/// 5522 通道：读一开始，光标处立刻是占位符（号码此刻就定了），字节还没到。
#[test]
fn an_image_read_puts_a_placeholder_at_the_cursor() {
    let mut app = test_app();
    app.input.set_text("see ");
    let target = start_image_read(&mut app);
    assert_eq!(target.number, 1);
    assert_eq!(app.input.text(), "see [Image #1 ⠋]");
    assert!(app.input.has_unsettled_images(), "在途 = 提交门关着");
}

/// 进度是**就地**的：标签变长，位置与号码不动，用户在 chip 后面打字的落点也不漂。
#[test]
fn progress_rewrites_the_placeholder_in_place() {
    let mut app = test_app();
    start_image_read(&mut app);
    app.input.insert_str("note");
    assert_eq!(app.input.cursor_col, "[Image #1 ⠋]note".chars().count());

    app.handle_clipboard(ClipboardEvent::ReadProgress {
        received: 12_582_912,
    });
    assert_eq!(app.input.text(), "[Image #1 ⠋ ↓12.0MB]note");
    assert_eq!(
        app.input.cursor_col,
        "[Image #1 ⠋ ↓12.0MB]note".chars().count(),
        "光标还在原地（数据不动位置）"
    );

    // 陈旧句柄（草稿被清空）之后的进度什么都不做，也不 panic。
    app.input.clear();
    app.handle_clipboard(ClipboardEvent::ReadProgress { received: 99 });
    assert_eq!(app.input.text(), "");
}

/// 字节到手 → 「处理中」；压缩完成 → 同一处翻成正式 chip（号码 = 粘贴序）。
#[test]
fn the_placeholder_settles_in_place_after_compression() {
    let mut app = test_app();
    app.input.set_text("pic: ");
    start_image_read(&mut app);
    app.handle_clipboard(ClipboardEvent::ReadProgress { received: 4096 });
    assert_eq!(app.input.text(), "pic: [Image #1 ⠋ ↓4KB]");
    assert!(app.input.has_unsettled_images(), "字节还没到：门关着");

    deliver_image(&mut app, png(64, 32, [200, 30, 30]), None);
    assert_eq!(app.input.text(), "pic: [Image #1 ⠋ 处理中…]");
    assert!(
        app.pending_clipboard_images().is_empty(),
        "队首已经出队去压了"
    );

    let (target, bytes, options) = take_compression(&mut app);
    assert_eq!(target.number, 1, "压缩任务认领的是**那个**占位符");
    assert_eq!(options, app.config.paste.image.compress_options());
    assert_eq!(image_dimensions(&bytes), (64, 32), "intent 带的是原始字节");

    let image = compress(&bytes, &options).expect("compress");
    app.on_clipboard_compressed(target, Ok(image.clone()));
    assert_eq!(app.input.text(), format!("pic: {}", chip_text(1, &image)));
    assert!(!app.input.has_unsettled_images(), "落定 = 门开了");
    assert!(app.toast.is_none(), "成功不 toast：chip 就是反馈");
    assert!(
        !app.composer_typing_blocked(),
        "图片粘贴不改键盘归属（草稿照常可编辑）"
    );
}

/// PNG 的 IHDR 尺寸（不依赖压缩器：断言 intent 里的原始字节就是用户粘的那张）。
fn image_dimensions(bytes: &[u8]) -> (u32, u32) {
    let decoded = image::load_from_memory(bytes).expect("decode the fixture");
    (decoded.width(), decoded.height())
}

/// 读失败：占位符**原地**变 `✗`（不消失），toast 说原因；门随之打开。
#[test]
fn a_read_failure_flips_the_placeholder_to_failed() {
    let mut app = test_app();
    app.input.set_text("pic: ");
    start_image_read(&mut app);
    app.handle_clipboard(ClipboardEvent::ReadFailed {
        mime: "image/png".into(),
        failure: ClipboardFailure::TooLarge,
    });
    assert_eq!(app.input.text(), "pic: [Image #1 ✗]");
    assert!(app.image_reads.is_empty(), "在途目标已收场");
    assert!(!app.input.has_unsettled_images(), "失败不拦发送");
    let toast = app.toast.as_ref().expect("失败要可见");
    assert!(
        toast.message.contains("too large"),
        "文案来自 ClipboardFailure::message：{toast:?}"
    );
}

/// 用户在传输途中把占位符删了：那张图**不要了** —— 字节到时连压缩都不跑，
/// 更不会重新插一个 chip（「删占位符 = 这张我不要了」）。
#[test]
fn deleting_the_placeholder_drops_the_paste_in_flight() {
    let mut app = test_app();
    let target = start_image_read(&mut app);
    app.input.backspace();
    assert_eq!(app.input.text(), "");

    deliver_image(&mut app, png(8, 8, [3, 3, 3]), None);
    assert!(
        app.drain_intents().is_empty(),
        "没有落点的字节不该再花 CPU 去压"
    );
    assert!(app.pending_clipboard_images().is_empty());
    assert_eq!(app.input.text(), "", "回执被丢弃，不重新插入");
    assert!(app.toast.is_none(), "这不是失败：用户自己的动作");
    // 句柄已经从在途队列里出队（交付消费掉了它）。
    assert!(app.image_reads.is_empty());
    let _ = target;
}

/// 草稿被清空（Esc）之后，在途的字节与压缩回执都不许落进新草稿：
/// 号码会从头计，同号 chip 是**别人**的。
#[test]
fn a_paste_in_flight_never_lands_in_a_new_draft() {
    let mut app = test_app();
    start_image_read(&mut app);
    // 用户在传输途中清空草稿（Esc）。
    app.input.clear();
    // 老读的字节随后到齐：占位符已经不在了 ⇒ 字节丢掉（不压、不插）。
    deliver_image(&mut app, png(8, 8, [3, 3, 3]), None);
    assert!(app.drain_intents().is_empty(), "回到旧草稿的字节没有落点");
    assert_eq!(app.input.text(), "", "老读不许落进新草稿");
    assert!(app.toast.is_none(), "这不是失败：用户自己的动作");

    // 新的一次粘贴（协议机在旧读收场后才开始它）：号码从头计，不受老读影响。
    let fresh = start_image_read(&mut app);
    assert_eq!(fresh.number, 1, "号码确实重来了");
    deliver_image(&mut app, png(8, 8, [5, 5, 5]), None);
    let (target, bytes, options) = take_compression(&mut app);
    assert_eq!(target, fresh, "配对到的是新读自己的占位符");
    let image = settle(&mut app, target, &bytes, &options);
    assert_eq!(app.input.text(), chip_text(1, &image));
}

/// **跨通道**：5522 的读还在飞时用本地通道（Ctrl+V）粘一张图 —— 它必须落进
/// **自己的**新占位符，而不是抢走那条 5522 读的（否则两张图的号码与位置全错，
/// 而且 5522 那张会在字节到时新建 chip）。
#[test]
fn a_local_paste_never_claims_the_read_in_flight() {
    let mut app = test_app();
    app.input.set_text("a ");
    let remote = start_image_read(&mut app);
    assert_eq!(remote.number, 1);

    // 本地通道交付一张图（读回字节才知道是图片）：新占位符 #2。
    app.on_clipboard_paste(Ok(PasteOutcome::Image {
        bytes: png(8, 8, [1, 1, 1]),
        mime: "image/png".into(),
    }));
    assert_eq!(app.input.text(), "a [Image #1 ⠋][Image #2 ⠋ 处理中…]");
    assert_eq!(app.image_reads.front(), Some(&remote), "在途读纹丝不动");

    // 本地那张压完：落进 #2（不是 #1）。
    let (target, bytes, options) = take_compression(&mut app);
    assert_eq!(target.number, 2);
    let local = settle(&mut app, target, &bytes, &options);
    assert_eq!(
        app.input.text(),
        format!("a [Image #1 ⠋]{}", chip_text(2, &local))
    );

    // 5522 那张随后到齐：落进它自己的 #1。
    deliver_image(&mut app, png(8, 8, [2, 2, 3]), None);
    let (target, bytes, options) = take_compression(&mut app);
    assert_eq!(target, remote);
    let remote_image = settle(&mut app, target, &bytes, &options);
    assert_eq!(
        app.input.text(),
        format!("a {}{}", chip_text(1, &remote_image), chip_text(2, &local))
    );
}

/// 队列是 FIFO：两次粘贴按顺序压、按顺序落定 —— chip 号码 = 粘贴顺序。
#[test]
fn two_pastes_keep_their_order() {
    let mut app = test_app();
    paste_image(&mut app, png(8, 8, [1, 2, 3]));
    paste_image(&mut app, png(16, 16, [4, 5, 6]));
    assert_eq!(
        app.input.text(),
        "[Image #1 ⠋ 处理中…][Image #2 ⠋ 处理中…]",
        "两张的占位符都已经在草稿里（第二张等着压）"
    );
    assert_eq!(
        app.pending_clipboard_images().len(),
        1,
        "第二张等着第一张压完（单并发）"
    );

    let (target, bytes, options) = take_compression(&mut app);
    assert_eq!(target.number, 1);
    assert!(
        app.drain_intents().is_empty(),
        "在飞期间只有一条 intent（第二张还没出队）"
    );
    let first = compress(&bytes, &options).expect("first");
    app.on_clipboard_compressed(target, Ok(first.clone()));

    // 第一张落地的同时泵出第二张。
    let (target, bytes, options) = take_compression(&mut app);
    assert_eq!(target.number, 2, "压缩任务认领第二张的占位符");
    assert_eq!(app.pending_clipboard_images().len(), 0, "第二张也出队了");
    let second = compress(&bytes, &options).expect("second");
    app.on_clipboard_compressed(target, Ok(second.clone()));

    assert_eq!(
        app.input.text(),
        format!("{}{}", chip_text(1, &first), chip_text(2, &second)),
        "号码随粘贴顺序，不是随压缩完成的顺序"
    );
    assert!(app.drain_intents().is_empty(), "队列空了就不再泵");
}

/// 压缩失败：toast 说明原因，占位符原地变 `✗`，流水线继续（下一张照压）。
#[test]
fn a_picture_that_cannot_be_compressed_fails_in_place() {
    let mut app = test_app();
    paste_image(&mut app, vec![0x00, 0x01, 0x02, 0x03]);
    paste_image(&mut app, png(8, 8, [9, 9, 9]));
    let (target, _, _) = take_compression(&mut app);

    // 第一张不是图片：压缩器回 Err（真跑一遍，不假装）。
    let options = app.config.paste.image.compress_options();
    let failed =
        compress(&[0x00, 0x01, 0x02, 0x03], &options).expect_err("garbage must not decode");
    app.on_clipboard_compressed(target, Err(failed));

    let toast = app.toast.as_ref().expect("失败要可见");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert!(
        toast.message.starts_with("Image dropped:"),
        "文案要说清丢的是什么：{toast:?}"
    );
    assert_eq!(app.input.text(), "[Image #1 ✗][Image #2 ⠋ 处理中…]");

    // 第二张照旧出队 → 压 → 落定（第一张的 ✗ 留在原地）。
    let (target, bytes, options) = take_compression(&mut app);
    let image = compress(&bytes, &options).expect("second");
    app.on_clipboard_compressed(target, Ok(image.clone()));
    assert_eq!(
        app.input.text(),
        format!("[Image #1 ✗]{}", chip_text(2, &image))
    );
}

/// 会话切换不该丢掉压缩回执：草稿还在，chip 就还得落（`handle_fetch_result` 的守卫
/// 对这条回执不生效）。丢掉它流水线会永远停在「在飞」，之后所有粘贴都不落地。
#[test]
fn a_compression_result_survives_a_session_switch() {
    let mut app = test_app();
    let options = app.config.paste.image.compress_options();
    paste_image(&mut app, png(8, 8, [3, 3, 3]));
    let (target, queued, _) = take_compression(&mut app);

    // 会话换了（压缩还在飞）。
    app.session_id = "another-session".into();
    let compressed = compress(&queued, &options).expect("compress");
    app.handle_fetch_result(crate::app::intent::FetchResult {
        session_id: "test-session".into(),
        payload: crate::app::intent::FetchPayload::ClipboardImage {
            target,
            result: Ok(compressed.clone()),
        },
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
    paste_image(&mut app, vec![1, 2, 3]);
    assert!(
        app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::CompressClipboardImage { .. })),
        "字节进了流水线，不是被面板吃掉"
    );
    assert_eq!(app.input.text(), "[Image #1 ⠋ 处理中…]", "占位符照落");
}

/// 队列上限（条数）：到顶丢最旧的一张，用户刚粘的那张永远留着；**丢弃要出声**
/// （红线：不出现「粘了但什么都没发生」——这里与协议机队列满走同一条反馈），
/// 被丢的那张的占位符同时也翻成 `✗`。
#[test]
fn the_queue_is_capped_by_count() {
    let mut app = test_app();
    // 只填不泵：把「在飞」占住，后面的全留在队列里。
    app.clipboard_compress_in_flight = true;
    app.image_reads.clear();
    for i in 0..(MAX_PENDING_IMAGES + 3) {
        paste_image(&mut app, vec![i as u8; 4]);
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

    // 被丢的那三张的占位符翻成 ✗，留在草稿里（它们是用户看得见的东西）。
    let text = app.input.text();
    assert_eq!(text.matches('✗').count(), 3, "{text}");
    assert_eq!(
        text.matches("处理中…").count(),
        MAX_PENDING_IMAGES,
        "留下的那些还在等压缩：{text}"
    );
}

/// 队列上限（总字节）：一次 64 MiB 级的大图不会把内存顶穿；丢弃同样有 toast。
#[test]
fn the_queue_is_capped_by_bytes() {
    let mut app = test_app();
    app.clipboard_compress_in_flight = true;
    let bite = 24 * 1024 * 1024; // 24 MiB × 4 张 > 64 MiB 上限
    for i in 0..4u8 {
        paste_image(&mut app, vec![i; bite]);
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
    app.handle_clipboard(ClipboardEvent::Delivered(ClipboardPayload::Text(
        "see this ".into(),
    )));
    paste_image(&mut app, png(8, 8, [1, 1, 1]));
    let (target, bytes, options) = take_compression(&mut app);
    let image = settle(&mut app, target, &bytes, &options);
    app.handle_clipboard(ClipboardEvent::Delivered(ClipboardPayload::Text(
        "and this".into(),
    )));

    assert_eq!(
        app.input.text(),
        format!("see this{}and this", chip_text(1, &image)),
        "chip 内联在光标处，后面的文本留在同一行"
    );
}

// ── 提交门：在途图片不许被留在草稿里 ─────────────────────────────────

/// 在途图片拦下 Enter：不发送、不丢草稿，并给一句「怎么继续」。
#[test]
fn enter_is_blocked_while_an_image_is_in_flight() {
    let mut app = test_app();
    app.input.set_text("look");
    start_image_read(&mut app);

    app.handle_key(key(crossterm::event::KeyCode::Enter));
    assert!(
        !app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::SendMessage { .. })),
        "在途图片期间不许发送"
    );
    assert!(app.input.text().contains("look"), "草稿一个字都不能少");
    let toast = app.toast.as_ref().expect("拦下要说一声");
    assert!(
        toast.message.contains("delete the chip"),
        "提示要带出口：{toast:?}"
    );
}

/// 门是**删掉占位符**就开的：用户明确表示不要这张图。
#[test]
fn deleting_the_placeholder_lets_enter_through_again() {
    let mut app = test_app();
    app.input.set_text("look");
    start_image_read(&mut app);
    // 光标在 chip 之后：退格吃掉整块 chip（原子删除）。
    app.handle_key(key(crossterm::event::KeyCode::Backspace));
    assert_eq!(app.input.text(), "look");

    app.handle_key(key(crossterm::event::KeyCode::Enter));
    let sent = app
        .drain_intents()
        .into_iter()
        .find(|i| matches!(i, AppIntent::SendMessage { .. }));
    let Some(AppIntent::SendMessage {
        content, images, ..
    }) = sent
    else {
        panic!("删掉占位符之后 Enter 必须照常发送");
    };
    assert_eq!(content, "look");
    assert!(images.is_empty());
}

/// 只剩一张 `✗`（失败占位符）的草稿：Enter 与空草稿同样静默无操作 —— 不许发出
/// 空消息（正文与图片都空），也不许把用户丢图的唯一痕迹抹掉。
#[test]
fn enter_on_a_failed_chip_only_draft_sends_nothing() {
    let mut app = test_app();
    start_image_read(&mut app);
    app.handle_clipboard(ClipboardEvent::ReadFailed {
        mime: "image/png".into(),
        failure: ClipboardFailure::Timeout,
    });
    assert_eq!(app.input.text(), "[Image #1 ✗]");

    app.handle_key(key(crossterm::event::KeyCode::Enter));
    assert!(
        !app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::SendMessage { .. })),
        "没有正文也没有图片：不发"
    );
    assert_eq!(app.input.text(), "[Image #1 ✗]", "草稿照旧（✗ 还在）");
}

/// 有正文时 `✗` 不拦发送：消息照发，草稿连同 `✗` 一起清掉。
#[test]
fn a_failed_chip_does_not_hold_a_real_message_back() {
    let mut app = test_app();
    app.input.set_text("note ");
    start_image_read(&mut app);
    app.handle_clipboard(ClipboardEvent::ReadFailed {
        mime: "image/png".into(),
        failure: ClipboardFailure::Timeout,
    });

    app.handle_key(key(crossterm::event::KeyCode::Enter));
    let sent = app
        .drain_intents()
        .into_iter()
        .find(|i| matches!(i, AppIntent::SendMessage { .. }));
    let Some(AppIntent::SendMessage {
        content, images, ..
    }) = sent
    else {
        panic!("有正文就要照常发送");
    };
    assert_eq!(content, "note");
    assert!(images.is_empty(), "失败的图片没有载荷可发");
}

/// 落定的图片照常发送（门只拦在途的那些）。
#[test]
fn a_settled_image_does_not_block_enter() {
    let mut app = test_app();
    app.input.set_text("here: ");
    start_image_read(&mut app);
    deliver_image(&mut app, png(8, 8, [7, 7, 7]), None);
    let (target, bytes, options) = take_compression(&mut app);
    let image = settle(&mut app, target, &bytes, &options);

    app.handle_key(key(crossterm::event::KeyCode::Enter));
    let sent = app
        .drain_intents()
        .into_iter()
        .find(|i| matches!(i, AppIntent::SendMessage { .. }));
    let Some(AppIntent::SendMessage {
        content, images, ..
    }) = sent
    else {
        panic!("落定的图片不该拦发送");
    };
    assert_eq!(content, "here:");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].bytes.len(), image.bytes.len());
}

/// runner 的阻塞池那一跳：`compress_clipboard_image` 在真 tokio 运行时里把一张 PNG
/// 变成 JPEG 产物，错误面转成给 toast 的一句话。
///
/// 直接驱动抽出来的函数而不是 `execute_intent`：后者需要 `WingTerminal`（真实
/// stdout），单测造不出来（同 `deliver_user_message` 的抽取理由）。
#[tokio::test]
async fn the_runner_compresses_on_the_blocking_pool() {
    let mut app = test_app();
    let target = start_image_read(&mut app);
    deliver_image(&mut app, png(48, 24, [10, 200, 10]), None);
    let (intent_target, bytes, options) = take_compression(&mut app);
    assert_eq!(intent_target, target);

    let image = super::super::runner::compress_clipboard_image(bytes, options)
        .await
        .expect("compress");
    assert_eq!(image.bytes[..2], [0xFF, 0xD8], "JPEG SOI");
    assert_eq!((image.width, image.height), (48, 24));
    app.on_clipboard_compressed(target, Ok(image));
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
///
/// 这条通道**没有「读开始」信号**（读回字节才知道是图片），所以占位符在字节到手那
/// 一刻以「处理中」出现 —— 与 5522 通道殊途同归。
#[test]
fn local_paste_image_joins_the_pipeline() {
    let mut app = test_app();
    app.on_clipboard_paste(Ok(PasteOutcome::Image {
        bytes: vec![1, 2, 3],
        mime: "image/png".into(),
    }));
    assert_eq!(app.input.text(), "[Image #1 ⠋ 处理中…]");
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
