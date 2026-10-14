//! OSC 5522 粘贴通道的 App 边界（`app/clipboard.rs`）。
//!
//! 断言的是**接线语义**：文本与 bracketed paste 同一条路（穿过模态挡板），图片「不丢」
//! 地暂存，失败只弹 toast 不动内容。

use crate::app::tests::support::test_app;
use crate::tui::ClipboardFailure;
use crate::tui::ClipboardPayload;
use crate::ui::toast::ToastKind;

fn image_payload() -> ClipboardPayload {
    ClipboardPayload::Image {
        bytes: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
        mime: "image/png".into(),
        name: None,
    }
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
fn image_payload_is_stashed_not_dropped() {
    let mut app = test_app();
    app.handle_clipboard(Ok(image_payload()));
    let stashed = app.pending_clipboard_images();
    assert_eq!(stashed.len(), 1, "图片必须留下（05 接手）");
    assert_eq!(stashed[0].bytes, vec![0x89, b'P', b'N', b'G', 1, 2, 3]);
    assert_eq!(stashed[0].mime, "image/png");
    assert_eq!(stashed[0].name, None);
    let toast = app.toast.as_ref().expect("告知用户");
    assert_eq!(toast.kind, ToastKind::Info);
    assert!(
        toast.message.contains("image/png"),
        "toast 带上 MIME：{toast:?}"
    );
    assert_eq!(
        app.input.text(),
        "",
        "图片不进文本草稿（顺序不承诺，chip 归 05）"
    );
}

#[test]
fn two_images_accumulate() {
    let mut app = test_app();
    app.handle_clipboard(Ok(image_payload()));
    app.handle_clipboard(Ok(image_payload()));
    assert_eq!(app.pending_clipboard_images().len(), 2, "多次粘贴累积");
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

/// 图片暂存不影响输入提示与焦点状态（边界只加数据，不改 UI 模式）。
#[test]
fn image_stash_keeps_the_composer_usable() {
    let mut app = test_app();
    app.handle_clipboard(Ok(image_payload()));
    app.handle_clipboard(Ok(ClipboardPayload::Text("after".into())));
    assert_eq!(app.input.text(), "after");
    assert_eq!(app.pending_clipboard_images().len(), 1);
}
