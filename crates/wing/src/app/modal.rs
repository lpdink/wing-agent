//! Modal ownership lane — who owns the keyboard, and the Escape ladder.
//!
//! The TUI has three modal layers (ask panel, `/model` picker, command candidate popup) plus the
//! composer. This module declares, **once**, which one owns a key event:
//!
//! * [`App::modal_chain`] — the layers that are up, in priority order (the declaration order of
//!   [`ModalOwner`] *is* the priority order);
//! * [`App::route_key`] — the key → owner decision, including the keys the app always keeps for
//!   itself (Esc, page keys, Ctrl+C). A layer that does not take a key lets it through to the next
//!   one, and the chat keeps its scroll keys all the way down;
//! * [`App::handle_key`] — a thin ladder that dispatches each route to its handler.
//!
//! The mouse path reads the same declaration ([`App::composer_pointer_blocked`]) instead of
//! re-deriving "is a modal up?" on its own, so the two input channels cannot drift apart.
//!
//! The panel **state machines** (ask panel questions, model picker pages) are neutral and live in
//! `shared/panels/`; what lives here is their App-side lifecycle: the queues, the chat-cell
//! mirroring, and the reply/apply paths.
//!
//! Call directions: [`super::commands`] calls in for the `/model` picker and the composer's submit
//! path; [`super::projection`] calls in for ask registration; this module calls [`super::commands`]
//! (submit / popup refresh).

use super::App;
use super::AppIntent;
use super::commands::frontend_command;
use super::commands::image_dropped_toast;
use crate::shared::panels::ask::AskPanel;
use crate::shared::panels::ask::PanelAction;
use crate::shared::panels::picker::ModelPanel;
use crate::shared::panels::picker::ModelPanelAction;
use crate::tui::is_quit_key;
use crate::ui::input_area::InputAction;
use crate::ui::input_area::SubmitPayload;
use crate::ui::popup::ActivePopup;
use crate::ui::popup::command::candidate_request_for;
use crate::ui::popup::command::is_must_select_command;
use crate::ui::popup::command::parse_slash_input;
use crate::ui::toast::Toast;

/// The modal layer that owns the keyboard right now.
///
/// **Declaration order is the priority order** — see [`App::modal_owner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModalOwner {
    /// 设置面板（`/settings`）：**最高优先级**，且是唯一一个连应用保留键
    /// （`Esc` / `PageUp` / `PageDown` / `Ctrl+O`）也接管的层。`Ctrl+C`
    /// 永远归应用。面板关闭的动作由 App 执行（面板只产出 `SettingsAction`）。
    Settings,
    /// Ask panel (queue front): swallows every key except the ones the app
    /// reserves (`Esc`, `PageUp` / `PageDown`). Every ask is a panel — the
    /// retired menu's own layer is gone.
    AskPanel,
    /// `/model` picker: swallows every key except the page keys.
    ModelPicker,
    /// Command candidate popup: only the navigation keys.
    Popup,
}

/// A key the chat viewport owns even while the composer has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChatScrollAction {
    /// One viewport page up.
    PageUp,
    /// One viewport page down.
    PageDown,
    /// One line up (Ctrl+Up).
    LineUp,
    /// One line down (Ctrl+Down).
    LineDown,
    /// Jump to the top (Ctrl+Home).
    Top,
    /// Jump to the bottom (Ctrl+End).
    Bottom,
}

/// Where a key event goes, decided before anything is mutated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyRoute {
    /// Ctrl+C — the double-press quit gesture.
    Quit,
    /// Ctrl+O — 展开 / 收起思考块（**全局**：所有轮一起切；应用保留键，面板之下也可用）。
    ToggleReasoning,
    /// `Ctrl+V` / `super+v` — 读系统剪贴板（L1，见 `util::clipboard_paste`）。
    ///
    /// 应用保留键（与 Ctrl+O 同档）：读取在后台任务上跑，**落点**才做门控 —
    /// 文本走 `App::handle_paste`（模态挡板自动继承），图片走 `on_clipboard_local_image`
    /// （不受挡板，与 OSC 5522 通道同一条既定取舍）。
    PasteClipboard,
    /// 设置面板 owns the key（`Esc` / `PageUp` / `PageDown` / `Ctrl+O` 一并交给它；
    /// `Ctrl+C` 在它之前就被应用截走）。
    Settings,
    /// The ask panel owns the key.
    AskPanel,
    /// The `/model` picker owns the key.
    ModelPicker,
    /// Escape — the app's own ladder (popup → draft → interrupt).
    EscLadder,
    /// Command popup navigation (`Up` / `Down` / `Tab` / `Enter`); keys the
    /// popup does not consume go on to the composer.
    PopupNav,
    /// The popup is armed but has no candidates: `Enter` on a must-select
    /// command is refused with a toast, otherwise the popup is dropped and the
    /// key continues to the composer.
    PopupEmpty,
    /// 会话面板的层视图：`→` 进层 / `←`（含层 ≥ 1 的 `Esc`）退层 / `Ctrl+A` dormant /
    /// `Ctrl+D`、`Ctrl+U` 翻页。判据见 [`App::route_key`]。
    SessionLevel(SessionLevelAction),
    /// The chat viewport owns the key.
    ChatScroll(ChatScrollAction),
    /// Everything else — the composer.
    Composer,
}

/// Keys the app keeps for itself while a modal is up.
fn app_reserved_key(key: &crossterm::event::KeyEvent) -> bool {
    matches!(
        key.code,
        crossterm::event::KeyCode::Esc
            | crossterm::event::KeyCode::PageUp
            | crossterm::event::KeyCode::PageDown
    )
}

/// 会话面板层视图接管的键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionLevelAction {
    /// `→` 进入高亮行的 crew（该行没有子会话 = no-op）。**不是 `Enter` 键**：
    /// `Enter` 在任何层都是「切到该行会话」。
    Descend,
    /// `←`（层 ≥ 1 的 `Esc` 同义）退回上一层；根层 = no-op。
    Back,
    /// `Ctrl+A` 翻转「当前层包含 dormant」。
    ToggleDormant,
    /// `Ctrl+D` / `Ctrl+U` 光标下 / 上移一屏。
    PageDown,
    PageUp,
}

/// 层视图的键位表。
///
/// 参数为空的会话面板是**另一套键位**：`←/→` 在那里是导航（进去 / 出来），不是
/// composer 的光标位置；`Esc` 在层 ≥ 1 时是「退回上一层」，不是关闭或打断。搜索态
/// （参数非空）一个键都不接管——`←/→` 得留给 composer，用户正在改搜索词。
///
/// 三个 `Ctrl` 组合（`A` / `D` / `U`）在 composer / popup 都没有别的含义（composer 的
/// Ctrl 只有 `Ctrl+J` / `Ctrl+M` 换行）；带 SHIFT / ALT / SUPER 的组合不接（系统手势，
/// 与 `is_paste_clipboard_key` 同一条口径）。
fn session_level_key(key: &crossterm::event::KeyEvent, depth: usize) -> Option<SessionLevelAction> {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;

    let control = key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::SUPER);
    match key.code {
        KeyCode::Right if key.modifiers.is_empty() => Some(SessionLevelAction::Descend),
        KeyCode::Left if key.modifiers.is_empty() => Some(SessionLevelAction::Back),
        KeyCode::Esc if depth > 0 => Some(SessionLevelAction::Back),
        KeyCode::Char('a' | 'A') if control => Some(SessionLevelAction::ToggleDormant),
        KeyCode::Char('d' | 'D') if control => Some(SessionLevelAction::PageDown),
        KeyCode::Char('u' | 'U') if control => Some(SessionLevelAction::PageUp),
        _ => None,
    }
}

/// `Ctrl+O` — 展开 / 收起思考块（**全局**：所有轮一起切）。
///
/// 应用保留键（面板之下也可用，同 Esc / Ctrl+C）：它是**查看**动作，不碰草稿、
/// 不碰面板状态。必须在这里截住 —— 放给 composer 就会被当普通字符 `o` 输入。
fn is_toggle_reasoning_key(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;

    matches!(key.code, KeyCode::Char('o' | 'O')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// `Ctrl+V` / `super+v` — 读系统剪贴板（L1）。
///
/// 两种解码形态都要接住：没开 kitty 键盘协议的终端把 Ctrl+V 发成控制字节 `0x16`
/// （crossterm → `Char('v') + CONTROL`）；开了协议的终端把 Cmd+V 回传成
/// `CSI 118;9u`（crossterm → `Char('v') + SUPER`，见 `tests/clipboard_paste_keys.rs`）。
///
/// 口径（两分支不对称是刻意的）：
///
/// - `super+v` 与 `super+shift+v`（macOS 的 Paste and Match Style）都触发粘贴 ——
///   SHIFT 只改变粘贴的“样式”，语义仍是粘贴；`super+alt/ctrl+v` 是与粘贴无关的
///   系统/终端组合，不接。
/// - `Ctrl+V` 触发；`Ctrl+Shift+V`（Linux 终端的粘贴默认键，动作发生在终端一侧）与
///   `Ctrl+Alt+V`（kitty 把 AltGr 报成 ctrl+alt）不接 —— 绑上只会吃掉字符输入或
///   得到「有时被终端吃掉、有时没有」的不一致。
fn is_paste_clipboard_key(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;

    matches!(key.code, KeyCode::Char('v' | 'V'))
        && if key.modifiers.contains(KeyModifiers::SUPER) {
            !key.modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
        } else {
            key.modifiers.contains(KeyModifiers::CONTROL)
                && !key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        }
}

/// The chat-scroll meaning of a key, if it has one.
///
/// Plain (unmodified) `Up` / `Down` are NOT scrolling keys: they belong to
/// whatever holds focus (panel navigation, or the composer cursor) — the wheel
/// has its own channel in `handle_mouse`.
fn chat_scroll_action(key: &crossterm::event::KeyEvent) -> Option<ChatScrollAction> {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;

    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::PageUp => Some(ChatScrollAction::PageUp),
        KeyCode::PageDown => Some(ChatScrollAction::PageDown),
        KeyCode::Up if control => Some(ChatScrollAction::LineUp),
        KeyCode::Down if control => Some(ChatScrollAction::LineDown),
        KeyCode::Home if control => Some(ChatScrollAction::Top),
        KeyCode::End if control => Some(ChatScrollAction::Bottom),
        _ => None,
    }
}

impl App {
    /// Modal layers that are up, highest priority first.
    ///
    /// The declaration order of [`ModalOwner`] **is** the priority order, and
    /// this is the single place where it is materialized: [`App::modal_owner`]
    /// takes the front, [`App::route_key`] walks the whole chain.
    fn modal_chain(&self) -> impl Iterator<Item = ModalOwner> {
        [
            self.settings_panel
                .is_some()
                .then_some(ModalOwner::Settings),
            (!self.ask_panels.is_empty()).then_some(ModalOwner::AskPanel),
            self.model_panel
                .is_some()
                .then_some(ModalOwner::ModelPicker),
            self.popup.active.is_active().then_some(ModalOwner::Popup),
        ]
        .into_iter()
        .flatten()
    }

    /// Which modal layer owns the keyboard right now (`None` = the composer).
    ///
    /// One declaration for every consumer: the keyboard ladder and the
    /// composer's pointer guard. An armed-but-invisible popup counts as a
    /// *keyboard* owner (its "no candidates" Enter behaviour is part of the
    /// composer's route), while the pointer guard additionally requires it to
    /// be visible — see [`App::composer_pointer_blocked`].
    pub(super) fn modal_owner(&self) -> Option<ModalOwner> {
        self.modal_chain().next()
    }

    /// Decide where a key goes — the single place where keyboard ownership is
    /// resolved.
    ///
    /// The modal chain is walked in priority order and **a layer that does not
    /// take the key lets it through to the next one**: the ask panel keeps the
    /// app-reserved keys (Esc, page keys) for whatever is below it, so a
    /// `/model` picker sitting underneath still gets its Esc — and the page
    /// keys still reach the chat.
    pub(super) fn route_key(&self, key: &crossterm::event::KeyEvent) -> KeyRoute {
        if is_quit_key(key) {
            return KeyRoute::Quit;
        }
        // 设置面板：最高优先级，且是**唯一接管应用保留键**的层——
        // `Esc` 走面板自己的 Esc 阶梯、`PageUp`/`PageDown` 翻面板的树、
        // `Ctrl+O` 交给面板（面板忽略它，比让它穿透去改看不见的聊天状态安全）。
        // 必须排在 `app_reserved_key` 与 Ctrl+O 的分支之前。
        if self.settings_panel.is_some() {
            return KeyRoute::Settings;
        }
        // Ctrl+O 与应用保留键同档：查看动作，任何面板之下都生效（也必须在
        // 面板之前截住 —— 落到 composer 会变成输入 `o`）。
        if is_toggle_reasoning_key(key) {
            return KeyRoute::ToggleReasoning;
        }
        // Ctrl+V / super+v 同样是应用保留键（设置浮层除外，早期分支已接管，与 Ctrl+O
        // 同档）：粘贴手势在其余面板之下也要接住（落点再做挡板），且必须赶在 composer
        // 之前 —— 否则 super+v 会被当字面 `v` 输入（Ghostty 图片剪贴板回传 super+v
        // 时的实测行为）。
        if is_paste_clipboard_key(key) {
            return KeyRoute::PasteClipboard;
        }
        for layer in self.modal_chain() {
            match layer {
                ModalOwner::AskPanel if !app_reserved_key(key) => return KeyRoute::AskPanel,
                ModalOwner::ModelPicker
                    if !matches!(
                        key.code,
                        crossterm::event::KeyCode::PageUp | crossterm::event::KeyCode::PageDown
                    ) =>
                {
                    return KeyRoute::ModelPicker;
                }
                // The popup's rungs sit below the Escape rung (Esc closes the
                // popup) and depend on whether it has candidates — see below.
                ModalOwner::Popup => break,
                _ => {}
            }
        }
        // The popup's own rungs: Esc closes it, the navigation keys are the
        // popup's, and the "armed but empty" case gets its own route so its
        // Enter refusal stays explicit. Scroll keys are *not* taken here — the
        // rungs below the popup handle them via `handle_scroll_or_composer_key`.
        //
        // 会话面板的层视图先说话：`→/←/Ctrl+A/Ctrl+D/Ctrl+U` 与层 ≥ 1 的 `Esc` 归
        // 面板。位置有讲究——必须在 Esc rung（否则 Esc 先被阶梯吃掉）与 popup rung
        // （否则空层会被 PopupEmpty 截走）之前。
        if self.popup.active.session_level_view()
            && let Some(action) = session_level_key(key, self.popup.level_view.depth())
        {
            return KeyRoute::SessionLevel(action);
        }
        if key.code == crossterm::event::KeyCode::Esc {
            return KeyRoute::EscLadder;
        }
        if self.popup.active.is_active() {
            return if self.popup.active.has_items() {
                KeyRoute::PopupNav
            } else {
                KeyRoute::PopupEmpty
            };
        }
        if let Some(action) = chat_scroll_action(key) {
            return KeyRoute::ChatScroll(action);
        }
        KeyRoute::Composer
    }

    /// Whether a keyboard-owning modal currently owns the composer pointer
    /// rules.
    ///
    /// The ask panel, the `/model` panel and the command candidate popup own
    /// the keyboard while they are up: their own inline editors are typing
    /// into the draft and their keys rewrite it. Placing the cursor — or worse,
    /// copying a fragment — under such a modal would race that flow, so
    /// composer pointer interaction is ignored outright.
    ///
    /// A popup only counts when it really takes over, which is not the same as
    /// `ActivePopup::is_active()`: a slash command that matches no candidate
    /// (`/zzz`) or whose candidates have not arrived yet leaves an *armed but
    /// invisible* popup (height 0 ⇒ it draws nothing, and plain keys already
    /// fall through to the composer). Blocking on that would swallow clicks in
    /// a completely idle-looking UI, so only a **visible** popup
    /// (`height() > 0`) counts — plus the itemless must-select case, whose
    /// Enter has to stay intercepted instead of reaching the composer.
    ///
    /// The chat band's drag selection and the wheel are **not** affected
    /// (rolling history while a panel is open has to keep working).
    /// 背景（chat band / 滚动条 / 状态栏）此刻是否**不接受指针**。
    ///
    /// 设置面板是浮层卡片：四周露出的聊天看得见但点不动 —— 三条理由，一条比一条硬：
    ///
    /// 1. 卡片盖住的链接 hit box 还在表里，点卡片 = 打开一个看不见的链接
    ///    （渲染侧另外 `mask_links(card)`，这里是同一件事的第二道门）；
    /// 2. 面板开着时选区**不画**（`App::draw` 的门控），拖出来的高亮看不见，
    ///    而 `capture_visible_rows` 也不跑 —— 复制到的会是打开面板前那一帧的快照；
    /// 3. 滚动条这一帧根本没画（hover / drag 态被清），点它会抓一个不存在的把手。
    ///
    /// 键盘侧的同类门是 [`Self::composer_pointer_blocked`]（那个只管 composer）。
    pub(super) fn background_pointer_blocked(&self) -> bool {
        self.settings_panel.is_some()
    }

    pub(super) fn composer_pointer_blocked(&self) -> bool {
        match self.modal_owner() {
            None => false,
            Some(ModalOwner::Popup) => {
                self.popup.active.height() > 0 || self.popup.active.is_must_select_empty()
            }
            Some(_) => true,
        }
    }

    /// Whether another layer is taking the composer's *keys* right now.
    ///
    /// Not the same question as [`App::composer_pointer_blocked`]: the command
    /// popup swallows only its navigation keys and lets everything else
    /// through, so typing still lands in the draft while a candidate list is up
    /// — the card must not go quiet for it. The ask panel and the model picker
    /// swallow every key, and then it must: the draft is frozen behind them.
    pub(super) fn composer_typing_blocked(&self) -> bool {
        matches!(
            self.modal_owner(),
            Some(ModalOwner::Settings | ModalOwner::AskPanel | ModalOwner::ModelPicker)
        )
    }

    /// Handle a terminal key event.
    pub(super) fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        match self.route_key(&key) {
            // Modal owners keep the Ctrl+C counter untouched: the gesture only
            // counts while the app itself is receiving keys.
            KeyRoute::Quit => self.handle_quit_key(key),
            KeyRoute::ToggleReasoning => {
                self.reset_quit_counter();
                self.toggle_reasoning_expansion();
            }
            KeyRoute::PasteClipboard => {
                self.reset_quit_counter();
                // 在飞合并：第一次读取落地前连按只算一次（同
                // `clipboard_compress_in_flight` 的纪律）——最坏 ~23 s 的读取窗口里
                // 堆 N 条子进程链只会换来 N 倍 churn 与 N 个收尾 toast。落地（成功
                // 或失败）由 runner 的开闸恢复可读。
                if crate::app::runner::claim_clipboard_read() {
                    self.push_intent(AppIntent::ReadClipboard);
                }
            }
            KeyRoute::Settings => self.handle_settings_key(key),
            KeyRoute::AskPanel => self.handle_ask_panel_key(key),
            KeyRoute::ModelPicker => self.handle_model_picker_key(key),
            KeyRoute::EscLadder => {
                // The Escape that merely closes the popup is the popup's, not
                // the app's: it leaves the quit gesture alone, exactly like the
                // modal owners above (pre-lane behaviour).
                if !self.handle_escape_key() {
                    self.reset_quit_counter();
                }
            }
            KeyRoute::PopupNav => {
                self.reset_quit_counter();
                // The popup takes only its navigation keys; everything else
                // falls through to the rungs below it.
                if !self.handle_popup_key(key) {
                    self.handle_scroll_or_composer_key(key);
                }
            }
            KeyRoute::PopupEmpty => {
                self.reset_quit_counter();
                // 例外：must-select 命令保持 popup——Enter 给显式"无匹配候选"反馈，
                // 绝不落到自由文本发送（参数必须来自候选）。层视图的空层不是这个意思
                // （那里没有参数可改，`←` 才是出口）：Enter 什么都不做，也不劝人改参数。
                if self.popup.active.is_must_select_empty() {
                    if key.code == crossterm::event::KeyCode::Enter {
                        if !self.popup.active.session_level_view() {
                            self.show_toast(Toast::warning(
                                "No matching candidates — adjust the argument and select from the popup",
                                std::time::Duration::from_secs(4),
                            ));
                        }
                        return;
                    }
                } else {
                    self.popup.close_active();
                }
                self.handle_scroll_or_composer_key(key);
            }
            KeyRoute::SessionLevel(action) => {
                self.reset_quit_counter();
                self.handle_session_level_key(action);
            }
            KeyRoute::ChatScroll(action) => {
                self.reset_quit_counter();
                self.handle_chat_scroll_key(action);
            }
            KeyRoute::Composer => {
                self.reset_quit_counter();
                self.handle_composer_key(key);
            }
        }
    }

    /// The rungs below the popup: the chat viewport keeps its scroll keys, and
    /// everything else is composer input.
    ///
    /// A popup only ever *declines* a key — it takes the navigation keys and
    /// nothing else, so page keys and Ctrl+Home/End must keep scrolling the
    /// chat while a candidate list is on screen (the wheel has the same
    /// contract on the mouse side). This is the old if-chain's order
    /// (popup → popup-empty → chat scroll → composer), kept explicit because
    /// the popup rung cannot forward to the chat rung through `route_key`.
    fn handle_scroll_or_composer_key(&mut self, key: crossterm::event::KeyEvent) {
        if let Some(action) = chat_scroll_action(&key) {
            self.handle_chat_scroll_key(action);
            return;
        }
        self.handle_composer_key(key);
    }

    /// Ctrl+O — 翻转思考块的展开（全局：所有轮一起切，会话内一直有效）。
    ///
    /// 初始默认（展开还是折叠）由 `rendering.thinking` 给；按下后的显式值
    /// 在会话内保留，resume / compaction 重建后依然生效。
    fn toggle_reasoning_expansion(&mut self) {
        let default_expanded =
            self.config.rendering.thinking == crate::config::rendering::ThinkingMode::Visible;
        if self.chat.toggle_thinking_expansion(default_expanded) {
            self.chat_dirty = true;
        }
    }

    /// Ctrl+C — first press warns, second press within 500 ms quits.
    fn handle_quit_key(&mut self, _key: crossterm::event::KeyEvent) {
        let now = std::time::Instant::now();
        if let Some(last) = self.ctrl_c_last
            && now.duration_since(last).as_millis() < 500
        {
            self.should_quit = true;
            return;
        }
        self.ctrl_c_last = Some(now);
        self.ctrl_c_count += 1;
        if self.ctrl_c_count == 1 {
            self.show_toast(Toast::warning(
                "Press Ctrl+C again to quit, Esc to interrupt",
                std::time::Duration::from_secs(2),
            ));
        }
    }

    /// Any key that reaches the app's own ladder breaks the quit gesture.
    fn reset_quit_counter(&mut self) {
        self.ctrl_c_count = 0;
        self.ctrl_c_last = None;
    }

    /// Ask panel: capture the key (queue front).
    ///
    /// Every ask is a panel now — the Bash confirmation included — so this is
    /// the only ask key handler. The app-reserved keys (`Esc`,
    /// `PageUp` / `PageDown`) never reach it: [`App::route_key`] keeps them.
    fn handle_ask_panel_key(&mut self, key: crossterm::event::KeyEvent) {
        let action = self
            .ask_panels
            .front_mut()
            .map(|panel| panel.handle_key(key))
            .unwrap_or(PanelAction::None);
        self.sync_front_panel();
        if let PanelAction::Reply(content) = action {
            self.finish_ask_panel(content);
        }
    }

    /// `/model` picker: modal while open.
    ///
    /// `Esc` closes the panel (it is not part of any turn, so it must NOT reach
    /// the interrupt ladder); `PageUp` / `PageDown` stay available for chat
    /// scrolling; every other key is consumed by the panel.
    fn handle_model_picker_key(&mut self, key: crossterm::event::KeyEvent) {
        let action = self
            .model_panel
            .as_mut()
            .map(|panel| panel.handle_key(key))
            .unwrap_or(ModelPanelAction::None);
        match action {
            ModelPanelAction::Apply { model_id } => {
                self.apply_selected_model(model_id);
            }
            ModelPanelAction::Cancel => {
                self.close_model_panel();
            }
            // Navigation keeps the panel open — mirror the new cursor /
            // page into the chat cell.
            ModelPanelAction::None => self.sync_model_panel_cell(),
        }
    }

    /// 设置面板：模态，最高优先级。
    ///
    /// 设置面板的状态机吃下按键并回一个 [`SettingsAction`]；这里只做"喂 + 分派"
    /// —— 全部分派逻辑（预览 / 保存 / 重启 / 重载 / 关闭）在 `app::settings`。
    /// `Ctrl+C` 永远到不了这里（[`App::route_key`] 先放行给应用）。
    fn handle_settings_key(&mut self, key: crossterm::event::KeyEvent) {
        let action = self
            .settings_panel
            .as_mut()
            .map(|panel| panel.handle_key(key))
            .unwrap_or(crate::shared::panels::settings::SettingsAction::None);
        self.handle_settings_action(action);
    }

    /// Escape ladder: close popup if active, otherwise clear/interrupt.
    ///
    /// Returns whether the Escape was the popup's alone: that path is the one
    /// Escape that does **not** reach the app's own ladder, so its caller must
    /// leave the Ctrl+C gesture alone (pre-lane behaviour, kept verbatim).
    fn handle_escape_key(&mut self) -> bool {
        if self.popup.active.is_active() {
            self.popup.close_active();
            return true;
        }
        if !self.input.text().is_empty() {
            self.input.clear();
            return false;
        }
        self.push_intent(AppIntent::InterruptSession);
        self.show_toast(Toast::info(
            "Interrupting agent...",
            std::time::Duration::from_secs(2),
        ));
        false
    }

    /// The chat viewport owns this key.
    fn handle_chat_scroll_key(&mut self, action: ChatScrollAction) {
        let page = self.geometry.chat_height().saturating_sub(2);
        match action {
            ChatScrollAction::PageUp => self.chat.page_up(page),
            ChatScrollAction::PageDown => self.chat.page_down(page, self.geometry.chat_height()),
            ChatScrollAction::LineUp => self.chat.scroll_up(1),
            ChatScrollAction::LineDown => self.chat.scroll_down(1, self.geometry.chat_height()),
            ChatScrollAction::Top => self.chat.jump_top(),
            ChatScrollAction::Bottom => self.chat.jump_bottom(),
        }
    }

    /// Everything else goes to the input area.
    fn handle_composer_key(&mut self, key: crossterm::event::KeyEvent) {
        // The *rendered* chrome, not the terminal width: the draft wraps at the
        // columns the card really has (the float and any squeeze included).
        let chrome = self.input.chrome();
        match self.input.handle_key(key, chrome) {
            InputAction::Submit(payload) => {
                // Submitting a message pins the view to the bottom so the
                // sent message and the agent's reply come into view.
                self.chat.jump_bottom();
                // must-select 命令：参数必须来自候选选择。popup 若被关闭
                //（如 Esc），重开 popup 而非发送自由文本——消灭未定义请求
                //（如 /fork 携带未经验证的 uuid）。草稿被整体换回文本：图片
                // 没有文本形态，因此这条路会丢图（`set_text` 的既定语义，
                // 见 `docs/dev/tui-input.md` 的「草稿被重写的路径」）。
                if let Some((cmd, _)) = parse_slash_input(&payload.text)
                    && is_must_select_command(cmd)
                {
                    self.input.set_text(&payload.text);
                    self.update_popup_from_input();
                    if self.popup.active.is_active() {
                        // 提交被拦下（等候选选择）而草稿已被换回纯文本：图片就丢在
                        // 这一下——和命令路径用同一句提示（见 `image_dropped_toast`）。
                        if !payload.images.is_empty() {
                            let command = frontend_command(&payload.text)
                                .map(|route| route.name)
                                .unwrap_or(cmd);
                            self.show_toast(image_dropped_toast(command));
                        }
                        return;
                    }
                    self.input.clear();
                }
                self.popup.close_active();
                self.submit_message(payload);
            }
            InputAction::SubmitBlocked => {
                // 草稿里还有在途的图片：提交会把这张图留在草稿里（或被静默吃掉）。
                // 拦下来，并把「怎么继续」一起说清：等它落地，或者删掉占位符。
                // 草稿一个字都没动（没有提交），所以这里也不碰 popup / 输入状态。
                self.show_toast(Toast::info(
                    "Image still uploading — delete the chip to send without it",
                    std::time::Duration::from_secs(3),
                ));
            }
            InputAction::Escape | InputAction::None => {
                // Update popup based on new text.
                self.update_popup_from_input();
            }
        }
        // NOTE: editing the composer must NOT yank the view back to the
        // bottom — the composer is a fixed block, and the user may be
        // reading history (e.g. item 456 of a review) while typing.
    }

    /// Handle popup navigation keys. Returns `true` if the key was consumed.
    fn handle_popup_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if !self.popup.active.is_active() || !self.popup.active.has_items() {
            return false;
        }
        match key.code {
            crossterm::event::KeyCode::Up => {
                self.popup.active.move_up();
                true
            }
            crossterm::event::KeyCode::Down => {
                self.popup.active.move_down();
                true
            }
            crossterm::event::KeyCode::Tab => {
                if let Some(completion) = self.popup.active.completion_text() {
                    self.input.set_text(&completion);
                    self.update_popup_from_input();
                }
                true
            }
            crossterm::event::KeyCode::Enter => {
                if self.popup.active.should_submit() {
                    // Complete the input from popup selection, then route through
                    // submit_message() — the single command dispatch path.
                    match &self.popup.active {
                        ActivePopup::Command {
                            rows,
                            state,
                            filter,
                        } if rows
                            .get(state.selected)
                            .is_some_and(|r| candidate_request_for(&r.name).is_none()) =>
                        {
                            let row = &rows[state.selected];
                            let typed_cmd = format!("/{}", filter);
                            if typed_cmd != row.name {
                                // The draft is rewritten, so its *expanded*
                                // text is what has to survive: a chip in the
                                // arguments stands for the pasted text, not
                                // for its label (the label would be sent as
                                // the command's argument).
                                let original = self.input.expand_and_get_text();
                                let args = original.find(' ').map(|p| &original[p..]).unwrap_or("");
                                self.input.set_text(&format!("{}{}", row.name, args));
                            }
                        }
                        _ => {
                            if let Some(completion) = self.popup.active.completion_text() {
                                self.input.set_text(&completion);
                            }
                        }
                    }
                    self.popup.close_active();
                    let text = self.input.expand_and_get_text();
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        // 命令补全只提交文本：改写草稿的那一步已经用文本重建过
                        // 草稿（图片没有文本形态，重写即丢，见 `set_text`）。
                        self.submit_message(SubmitPayload::text(text));
                    }
                    self.input.clear();
                } else {
                    if let Some(completion) = self.popup.active.completion_text() {
                        self.input.set_text(&completion);
                        self.update_popup_from_input();
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// 层切换是**对已抓取 payload 的重新投影**——零 HTTP 重取：面板已经开着，
    /// 输入路径的「打开即刷新」边沿（`was_session_panel → is_session_panel`）不会
    /// 再次触发，走的只是 `ActivePopup` 的重建。
    fn handle_session_level_key(&mut self, action: SessionLevelAction) {
        match action {
            SessionLevelAction::Descend => {
                let Some(id) = self.popup.active.selected_name().map(str::to_string) else {
                    return;
                };
                // 无子 = no-op（不进空层）。事实来自这次投影（内核的 crew 聚合：后代数
                // 为 0 ⟺ 没有直接子），不另算一遍拓扑——环 / 幽灵父上两套规则会分叉。
                let has_crew = self
                    .popup
                    .active
                    .selected_crew()
                    .is_some_and(|crew| crew.descendants > 0);
                if !has_crew {
                    return;
                }
                self.popup.level_view.enter(&id);
                self.reproject_session_panel();
                // 进入一层：光标落在当前会话那一行（不在本层则停在第一行）。
                self.popup.active.reselect(&self.session_id);
            }
            SessionLevelAction::Back => {
                let Some(left) = self.popup.level_view.back() else {
                    return;
                };
                self.reproject_session_panel();
                // 退回一层：光标落回你刚才进来的那一行（往返稳定，不跳顶部）。
                self.popup.active.reselect(&left);
            }
            SessionLevelAction::ToggleDormant => {
                let anchor = self.popup.active.selected_name().map(str::to_string);
                let index = self.popup.active.selected_index().unwrap_or(0);
                self.popup.level_view.toggle_dormant();
                self.reproject_session_panel();
                // 行集只会变大（打开）或收窄（关回去）：光标钉回原来那一行；那一行正好
                // 被收起时就停在它原来的位置上，而不是跳回第一条。
                let stayed = anchor.is_some_and(|anchor| self.popup.active.reselect(&anchor));
                if !stayed {
                    self.popup.active.select_index(index);
                }
            }
            SessionLevelAction::PageDown => self.popup.active.move_page(true),
            SessionLevelAction::PageUp => self.popup.active.move_page(false),
        }
    }

    /// 按当前的层状态重投影行集（不取数据）。
    fn reproject_session_panel(&mut self) {
        self.update_popup_from_input();
    }

    /// Handle a bracketed paste event: routed to the active ask panel's inline
    /// editor when one is up (panel is modal); the modal model panel and the
    /// settings overlay drop it; otherwise it goes to the composer.
    ///
    /// **顺序**：设置面板的挡板排在最前 —— 面板是模态浮层、拿走全部按键的层，
    /// 而队列里可能正躺着一条**看不见**的 ask（面板开着时到达的 ask 入队不弹）。
    /// 先问 ask 的话，`Ctrl+V` 会写进那条 ask 的内联编辑器（用户看不见自己改了什么）。
    pub(super) fn handle_paste(&mut self, text: &str) {
        // 设置面板：内联编辑器只吃按键事件，草稿在面板底下不可见 —— 粘贴进
        // composer 只会改掉用户关面板之后要发的内容。
        if self.settings_panel.is_some() {
            return;
        }
        if !self.ask_panels.is_empty() {
            if let Some(panel) = self.ask_panels.front_mut() {
                panel.insert_paste(text);
            }
            self.sync_front_panel();
            return;
        }
        // The model panel is modal and owns no text field — paste is dropped.
        if self.model_panel.is_some() {
            return;
        }
        self.input.insert_str(text);
        self.update_popup_from_input();
    }

    // -----------------------------------------------------------------------
    // Ask panels — queues, chat-cell mirroring, answers
    // -----------------------------------------------------------------------

    /// Clear all queued ask state and remove their cells from chat. Called when
    /// a turn ends or is interrupted — the backend cancels all feedback waiters
    /// at the same time.
    pub(super) fn clear_ask_state(&mut self) {
        if self.ask_panels.is_empty() {
            return;
        }
        for panel in self.ask_panels.drain(..) {
            self.chat.remove_ask(&panel.tool_call_id);
        }
        self.refresh_ask_placeholder();
    }

    /// Refresh the input placeholder to reflect the active (front) ask state.
    pub(super) fn refresh_ask_placeholder(&mut self) {
        if self.ask_panels.front().is_some() {
            self.input.placeholder = "Answering above · Esc to interrupt".into();
        } else {
            self.input.placeholder = "今天构建什么？".into();
        }
    }

    /// Register a normalized ask panel — the single registration entry, shared
    /// by the live projection and by resume replay.
    ///
    /// Interactive modes queue up (the front is the keyboard owner; answering
    /// routes to `post(tool_call_id)` and resolves the backend waiter). A
    /// [`PanelMode::Notice`](crate::shared::panels::ask::PanelMode::Notice) is
    /// display-only: it was rendered as a chat cell and must never take keys or
    /// answer, so it is not registered at all.
    ///
    /// The cell is the caller's business: the live path pushes it (and mirrors
    /// this very panel), replay has already pushed it in chain order.
    pub(super) fn register_ask_panel(&mut self, panel: AskPanel) {
        if panel.is_interactive() {
            self.ask_panels.push_back(panel);
        }
        self.refresh_ask_placeholder();
    }

    /// Sync the front panel's state into its chat cell (render snapshot).
    pub(super) fn sync_front_panel(&mut self) {
        if let Some(panel) = self.ask_panels.front() {
            let id = panel.tool_call_id.clone();
            let panel = panel.clone();
            self.chat.update_ask_panel(&id, panel);
        }
    }

    /// Send the front panel's final reply and pop it from the queue.
    pub(super) fn finish_ask_panel(&mut self, content: String) {
        let Some(panel) = self.ask_panels.pop_front() else {
            return;
        };
        let tool_call_id = panel.tool_call_id.clone();
        self.reply_to_ask(content, tool_call_id);
        self.refresh_ask_placeholder();
        // The agent resumes on this answer: the record stops being `blocked`
        // (the next queued ask, if any, keeps it blocked on that one).
        self.sync_program_status();
    }

    /// Answer a pending ask — the single reply path (every ask is a panel).
    ///
    /// The answer is a normal message carrying the ask's `tool_call_id` so the
    /// backend resolves its waiter. The content is whatever the panel built:
    /// `header: answer` lines for `Question` mode, the bare option label
    /// (`y` / `n` / `yolo`) for `RequiredChoice`.
    fn reply_to_ask(&mut self, content: String, tool_call_id: String) {
        self.push_intent(AppIntent::SendMessage {
            content,
            tool_call_id: Some(tool_call_id),
            request_id: crate::protocol::generate_request_id(),
            // Ask 的应答是纯文本（面板只产文本）。
            images: Vec::new(),
        });
    }

    // -----------------------------------------------------------------------
    // `/model` picker — App-side lifecycle
    // -----------------------------------------------------------------------

    /// Open the `/model` panel: cache-first (instant open, background refresh)
    /// when models were fetched before, fetch-first otherwise. Refused while a
    /// turn is running — the model must not change under an executing turn.
    pub(super) fn open_model_panel(&mut self) {
        if self.turn.working {
            self.show_toast(Toast::warning(
                "Can't switch model while the agent is working",
                std::time::Duration::from_secs(3),
            ));
            return;
        }
        if !self.model_sources.is_empty() {
            // Open now from cache; the fetch below refreshes in place.
            let panel = ModelPanel::new(self.model_sources.clone(), self.current_model_id());
            self.present_model_panel(panel);
            // The popup yields to the modal panel (never both at once).
            self.popup.close_active();
        } else {
            self.show_toast(Toast::info(
                "Loading models…",
                std::time::Duration::from_secs(2),
            ));
            self.model_panel_pending = true;
        }
        self.push_intent(AppIntent::FetchModels);
    }

    /// Show a freshly built picker panel: store it as the interactive state
    /// and render it at the tail of the transcript (like the Ask panel).
    pub(super) fn present_model_panel(&mut self, panel: ModelPanel) {
        self.model_panel_pending = false;
        self.chat.show_model_picker(panel.clone());
        self.model_panel = Some(panel);
    }

    /// Mirror the app-owned picker state into its chat cell (render snapshot).
    pub(super) fn sync_model_panel_cell(&mut self) {
        if let Some(panel) = self.model_panel.as_ref() {
            self.chat.update_model_picker(panel.clone());
        }
    }

    /// Close the picker without changing the model (Esc): drop both the
    /// interactive state and its transient cell.
    pub(super) fn close_model_panel(&mut self) {
        self.model_panel_pending = false;
        self.model_panel = None;
        self.chat.remove_model_picker();
    }

    /// The session's active model id — the reference word the picker
    /// preselects by and the `/api/session/update` call takes. `None` when the
    /// gateway could not resolve one (old session metadata / identify miss):
    /// the picker then opens without a mark rather than inventing an id.
    pub(super) fn current_model_id(&self) -> Option<&str> {
        self.status
            .model_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
    }

    /// The model declaration behind an id, as the display layer needs it:
    /// `(call_name, provider, label)`. `None` when the last `/api/models`
    /// snapshot does not cover the id (stale cache / id-less session).
    ///
    /// Display layer only: this never takes part in apply / matching / any
    /// identity decision (`ModelPanel::Apply` carries the id).
    pub(super) fn model_declaration(&self, model_id: &str) -> Option<(&str, &str, &str)> {
        let group = self
            .model_sources
            .iter()
            .find(|group| group.find(model_id).is_some())?;
        let detail = group.find(model_id)?;
        Some((
            detail.name.as_str(),
            group.provider.as_str(),
            detail.display_label(),
        ))
    }

    /// Declared display label for a model id, resolved from the last
    /// `/api/models` snapshot — the same declaration the picker rows render.
    /// `None` when the id is unknown **or** the label equals the id (an equal
    /// label adds no information and must not be shown twice).
    ///
    /// Display layer only: never part of apply / matching / any identity
    /// decision.
    pub(super) fn model_display_label(&self, model_id: &str) -> Option<String> {
        let (_, _, label) = self.model_declaration(model_id)?;
        (label != model_id).then(|| label.to_string())
    }

    /// Declared display label for a **call name** — the id-less fallback
    /// (legacy sessions / `identify` misses). Ambiguous across providers by
    /// nature: the first group carrying the call name wins. Display only.
    pub(super) fn model_display_label_by_name(&self, model: &str) -> Option<String> {
        let detail = self
            .model_sources
            .iter()
            .find_map(|group| group.models.iter().find(|detail| detail.name == model))?;
        let label = detail.display_label();
        (label != model).then(|| label.to_string())
    }

    /// Toast text for a model switch: the display label takes the first line,
    /// the raw reference word (model id) follows on its own line — **only**
    /// when a declared label exists (otherwise there is nothing to add: the
    /// first line already shows the id). This is the one place the raw id is
    /// allowed to reach the screen.
    pub(super) fn model_switch_toast(&self, model_id: &str) -> String {
        let provider = self
            .model_declaration(model_id)
            .map(|(_, provider, _)| provider);
        match (self.model_display_label(model_id), provider) {
            (Some(label), Some(p)) => format!("Model: {label} ({p})\n↳ {model_id}"),
            (Some(label), None) => format!("Model: {label}\n↳ {model_id}"),
            (None, Some(p)) => format!("Model: {model_id} ({p})"),
            (None, None) => format!("Model: {model_id}"),
        }
    }

    /// Apply the model id chosen in the panel: close it, dispatch the id
    /// verbatim and give immediate feedback. Refuses while a turn is running
    /// (defense-in-depth — the panel is already guarded against opening
    /// mid-turn, but a race via SyncSession / fork could start a turn while
    /// the panel is visible).
    pub(super) fn apply_selected_model(&mut self, model_id: String) {
        if self.turn.working {
            self.close_model_panel();
            self.show_toast(Toast::warning(
                "Can't switch model while the agent is working",
                std::time::Duration::from_secs(3),
            ));
            return;
        }
        self.close_model_panel();
        let toast = self.model_switch_toast(&model_id);
        self.push_intent(AppIntent::set_model(model_id));
        self.show_toast(Toast::info(toast, std::time::Duration::from_secs(3)));
    }
}
