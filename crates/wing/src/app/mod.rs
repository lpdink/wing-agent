//! Application state machine and main event loop.
//!
//! This file is the **composition root**: it holds the `App` state, runs the main loop and draws.
//! Everything else lives in a lane module with one responsibility:
//!
//! * [`commands`] — slash-command routing (table → handler → fetch projection);
//! * [`projection`] — gateway events projected into chat / turn / status;
//! * [`modal`] — keyboard ownership, the Escape ladder, panel lifecycles;
//! * [`frame`] — the geometry of the frame just drawn; [`mouse`] — pointer gesture routing (its
//!   priority chain declared once); [`selection_session`] — the drag's lifecycle (anchor,
//!   fingerprint, edge auto-scroll).
//!
//! State the UI renders as well (the selection panels, the shared magic strings) is neutral and
//! lives in [`crate::shared`] — this root only orchestrates it.

pub mod intent;
pub mod popup_state;
pub mod render_context;
pub mod replay;
pub mod runner;
pub mod transport;
pub mod turn_state;

mod clipboard;
mod commands;
mod frame;
mod images;
mod modal;
mod mouse;
mod projection;
mod selection_session;
mod settings;

pub use intent::AppIntent;

use std::sync::Arc;

use anyhow::Result;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::widgets::Widget as _;

use self::images::Images;
use self::transport::GatewayEndpoint;
use self::transport::Transport;
use self::transport::backoff;
use self::transport::connect_transport;
use crate::tui::TermEvent;
use crate::tui::WingTerminal;
use crate::ui::chat_view::ChatView;
use crate::ui::chat_view::ChatViewWidget;
use crate::ui::input_area::ActivityRail;
use crate::ui::input_area::ComposerWidget;
use crate::ui::input_area::InputArea;
use crate::ui::input_area::MetaRail;
use crate::ui::input_area::cursor_screen_pos;
use crate::ui::input_area::pointer;
use crate::ui::popup::selection::SelectionPopup;
use crate::ui::scrollbar;
use crate::ui::selection::SelectionRegion;
use crate::ui::status_bar::StatusBar;
use crate::ui::status_bar::StatusData;
use crate::ui::toast::Toast;
use crate::ui::toast::ToastKind;
use crate::ui::toast::render_toast;
use crate::ui::welcome::SessionFacts;
use crate::ui::welcome::Viewport;
use crate::ui::welcome::Welcome;
use crate::util::program_status;
use crate::util::title;
use program_status::BlockKind;
use program_status::Reporter;
use program_status::State;
use title::AttentionKind;

use self::frame::FrameGeometry;
use self::mouse::MouseOutcome;
use self::popup_state::PopupState;
use self::render_context::RenderContext;
use self::selection_session::SelectionSession;
use self::selection_session::selection_autoscroll_tick;
use self::turn_state::TurnState;

use crate::config::AppConfig;
use crate::config::ThemePalette;
use crate::shared::panels::ask::PanelMode;

/// Threshold for wide-mode status bar (shows cumulative usage details).
const WIDE_THRESHOLD: u16 = 100;

/// Lines scrolled per wheel event. Carried over from before alternate scroll
/// (#28) and validated then; per-event step is deliberately larger than the
/// 1-line keyboard step and smaller than a page. Real-terminal feel
/// (trackpad momentum vs. wheel notches) is calibrated by hand — see the
/// change's Open Questions.
const WHEEL_SCROLL_LINES: usize = 3;

/// Application state.
pub struct App {
    pub status: StatusData,
    /// 欢迎屏状态（tip 抽签 / 扫光时钟 / 上次构建宽度）。`None` = 已关掉
    /// （测试要一块干净的顶部，见 [`App::clear_welcome`]）。
    welcome: Option<Welcome>,
    pub chat: ChatView,
    pub input: InputArea,
    pub session_id: String,
    /// In-app text selection — one gesture, anchored in the *content*
    /// coordinates of the region it started in (chat band or composer), with
    /// its fingerprint, press-frame link and edge auto-scroll deadline, see
    /// [`crate::ui::selection`] and [`SelectionRegion`].
    selection: SelectionSession,
    /// Whether the app should exit.
    pub should_quit: bool,
    /// Pending side-effect intents. Drained by runner after each draw cycle.
    intents: Vec<AppIntent>,
    /// Force a full repaint on the next draw.
    ///
    /// ratatui is a diff-based renderer: it only repaints cells whose buffer
    /// value changed. When the physical terminal is disturbed out-of-band
    /// (focus regain, resize), its screen no longer matches ratatui's back
    /// buffer and the diff leaves stale glyphs behind ("rendering residue").
    /// Setting this flag makes the next draw call `Terminal::clear` first,
    /// which resets the back buffer so the whole screen is repainted.
    needs_full_redraw: bool,
    /// Chat (or any non-input UI state) changed since the last draw —
    /// coalesced by the frame gate.
    chat_dirty: bool,
    /// User input arrived — bypasses the frame gate (immediate draw).
    input_dirty: bool,
    /// Timestamp of the last executed draw (frame-gate reference).
    last_draw: std::time::Instant,
    /// Ctrl+C press count for double-press quit.
    ctrl_c_count: u8,
    /// Last Ctrl+C timestamp (for double-press detection).
    ctrl_c_last: Option<std::time::Instant>,
    /// Render context — tracks current turn's active cells.
    ctx: RenderContext,
    /// Popup state (active popup + candidate cache + dedup).
    pub(crate) popup: PopupState,
    /// Turn state (working flag + timer + spinner + usage).
    turn: TurnState,
    /// OSC 7501 program-status lane: formats reports, drops idempotent
    /// repeats, carries the `WING_PROGRAM_STATUS` switch.
    program_status: Reporter,
    /// Last tick instant for measuring real dt between TermEvent::Tick.
    last_tick: std::time::Instant,
    /// Active toast notification (lazy-expired in draw).
    toast: Option<Toast>,
    /// Queued ask panels (every ask — the retired Bash confirmation included;
    /// see `AskPanel::from_ask`). Concurrent asks queue up; the front entry is
    /// the active one.
    ask_panels: std::collections::VecDeque<crate::shared::panels::ask::AskPanel>,
    /// `/model` selection panel (None = closed). Modal while open.
    model_panel: Option<crate::shared::panels::picker::ModelPanel>,
    /// Last successful `/api/models` response — `/model` opens the panel from
    /// this cache instantly and refreshes in the background.
    model_sources: Vec<wing_api_client::models::ProviderModels>,
    /// `/model` was requested but the sources were not yet fetched (no cache
    /// at the time of request).  When true, a successful FetchModels result
    /// auto-opens the panel even though the user has not yet seen one.
    /// Cleared on any explicit panel close (Esc, apply) and on delivery.
    model_panel_pending: bool,
    /// Whether the terminal window/tab currently has focus.
    /// Default `true` — terminals that don't support focus events
    /// will never send FocusLost, so BEL is never triggered.
    focused: bool,
    /// User configuration.
    config: AppConfig,
    /// Resolved theme palette.
    palette: ThemePalette,
    /// Whether the gateway connection is active.
    connected: bool,
    /// TUI 启动时的工作目录，用于 /new 创建 session 时传递 workspace。
    launch_workspace: Option<String>,
    /// Geometry of the last drawn frame — the contract every input path
    /// (mouse, selection, page keys, composer editing width) reads. Written
    /// by `draw` alone; see [`FrameGeometry`].
    geometry: FrameGeometry,
    /// Overlay scrollbar interaction state (hover / drag).
    scrollbar: scrollbar::ScrollbarState,
    /// Markdown pictures: capability, store, metadata table, drawing pass —
    /// see [`images`](self::images).
    images: Images,
    /// 这个会话加载的 skills / rules 数量 —— 名牌右列的事实行。
    ///
    /// 由 SyncSession 的 agent 快照写入（`None` = 老网关 / 还没同步到），
    /// 读它的只有 [`App::sync_welcome`]：它曾是 transcript 顶部一条 system
    /// 消息，现在归名牌（见 [`crate::ui::welcome::SessionFacts`]）。
    session_facts: Option<SessionFacts>,
    /// OSC 5522 通道读回的图片（[`clipboard`] 边界）：压缩流水线的队列——
    /// 队首出队即交给后台压缩，所以这里只留还没轮到的那些。
    pending_clipboard_images: Vec<clipboard::PendingClipboardImage>,
    /// 是否有一张图正在压缩（单并发门）。它同时保证 **chip 插入序 = 粘贴序**：
    /// 一次只压一张，结果就按序回来（见 [`clipboard`]）。
    clipboard_compress_in_flight: bool,
    /// 5522 通道在途的图片读：每一条的占位符，按**开始序**排队。
    ///
    /// 协议机一次只服务一条读，所以队里通常恰好一个；用队列而不是单个 `Option`，是为了
    /// 让「Started 与交付 / 失败严格配对」这条不变量在 App 侧**显式**成立——配对错乱时
    /// 字节不会落进另一张图的 chip（交付取队首、进度看队首，一一对应）。
    ///
    /// 本地读取通道不走这里（那条通道读回字节才知道是图片，占位符在那一刻插）。
    image_reads: std::collections::VecDeque<crate::ui::input_area::ImageChipTarget>,
    /// 设置面板（`/settings`）：`Some` ⇔ 面板开着（`ModalOwner::Settings` 接管键盘）。
    /// 生命周期与分派全在 [`settings`] 模块。
    settings_panel: Option<crate::shared::panels::settings::SettingsPanel>,
    /// 面板的 Gateway 数据（`GET schema` + `GET get` 的一次成功快照）：
    /// cache-first 打开、overlay 的目录（详情栏）、保存后的回写都读它。
    settings_cache: Option<(
        wing_api_client::models::SettingsSchemaResponse,
        wing_api_client::models::SettingsGetResponse,
    )>,
    /// Interface 根的 catalog（`interface_catalog()`）；与注入面板的那一份相同，
    /// 供 overlay 的详情栏 / 色块预览用（07 没有"行 → 节点"的访问器，见 `shared::panels::settings`）。
    interface_catalog: Option<wing_api_client::models::SettingNode>,
    /// 打开面板时的 `config` 快照（实时预览的回退基准；保存成功 / `R` 重载后更新）。
    config_snapshot: Option<AppConfig>,
    /// 已请求、尚未返回的 `FetchSettings`（无缓存时到达后要自动打开面板；关面板即清）。
    settings_pending: bool,
    /// 已请求的 `R` 重载：结果到达时**无条件**应用快照（丢弃本地改动是它的语义），
    /// 而打开时的后台刷新只在面板干净时应用。
    settings_reload_pending: bool,
    /// 保存的两半：Interface 半边已定，Gateway 半边在飞（合并回执需要它）。
    settings_save: Option<settings::PendingSettingsSave>,
    /// 调色板刚变过（实时预览）：欢迎屏 header 的缓存键里没有调色板，
    /// `sync_welcome` 下一帧强制重建一次（同 Ctrl+O 需要显式作废 cell 缓存）。
    welcome_theme_dirty: bool,
    /// 开屏判定要用的牌子高度，按**内容宽**缓存（`(width, rows)`）。
    ///
    /// 判定必须先量牌子（按开屏宽度建一次 header），而矮终端上开屏装不下时每帧
    /// 都会退回会话版式 —— 不缓存的话每次 sync 都会把 header 重建两遍（扫光期
    /// ~25fps、每次击键各两遍）。牌子高度只由宽度决定（行数恒定），所以宽度是
    /// 完整的键。
    splash_probe: Option<(u16, u16)>,
}

impl App {
    /// An app with no image capability (`Images::disabled()`): the terminal is
    /// probed by [`run_app`], not here — a probe reads stdin (it must not race
    /// the event stream) and costs up to
    /// [`DEFAULT_DETECT_TIMEOUT`](crate::ui::image::DEFAULT_DETECT_TIMEOUT).
    pub fn new(session_id: String, config: AppConfig, launch_workspace: Option<String>) -> Self {
        Self::with_images(session_id, config, launch_workspace, Images::disabled())
    }

    /// [`App::new`] with an explicit image lane (the production path, and the
    /// test seam for an injected [`ImageSupport`](crate::ui::image::ImageSupport)).
    pub(crate) fn with_images(
        session_id: String,
        config: AppConfig,
        launch_workspace: Option<String>,
        images: Images,
    ) -> Self {
        let palette = ThemePalette::from_config(&config.colors);
        let max_input_lines = config.layout.max_input_lines;
        // 欢迎屏：tip 抽签 + 扫光时钟。这里先按"还没有帧"的宽度建一次，只是
        // 为了让 draw 之前 header 不为空 —— 第一次 draw 一定按真实的内容宽重建
        //（扫光未定格时 `needs_rebuild` 恒真）。
        let now = std::time::Instant::now();
        let mut welcome = Welcome::new(crate::shared::tips::seed_now(), now);
        let mut chat = ChatView::new();
        chat.set_header(welcome.build(
            &palette,
            Viewport::width_only(crate::ui::input_area::chrome::UNFRAMED_WIDTH),
            now,
            false,
            true,
            None,
        ));
        Self {
            status: StatusData::default(),
            welcome: Some(welcome),
            chat,
            input: InputArea::with_max_lines("今天构建什么？".into(), max_input_lines),
            session_id,
            selection: SelectionSession::default(),
            should_quit: false,
            intents: Vec::new(),
            needs_full_redraw: true,
            chat_dirty: false,
            input_dirty: false,
            last_draw: std::time::Instant::now(),
            ctrl_c_count: 0,
            ctrl_c_last: None,
            ctx: RenderContext::new(),
            popup: PopupState::default(),
            turn: TurnState::default(),
            program_status: Reporter::from_env(),
            last_tick: std::time::Instant::now(),
            toast: None,
            ask_panels: std::collections::VecDeque::new(),
            model_panel: None,
            model_sources: Vec::new(),
            model_panel_pending: false,
            focused: true,
            config,
            palette,
            connected: true,
            launch_workspace,
            // Nothing drawn yet: no band, no composer, no hit testing before
            // anything is on screen (the terminal width falls back to the
            // canonical 80 the composer's editor starts with).
            geometry: FrameGeometry::default(),
            scrollbar: scrollbar::ScrollbarState::default(),
            images,
            pending_clipboard_images: Vec::new(),
            clipboard_compress_in_flight: false,
            image_reads: std::collections::VecDeque::new(),
            session_facts: None,
            settings_panel: None,
            settings_cache: None,
            interface_catalog: None,
            config_snapshot: None,
            settings_pending: false,
            settings_reload_pending: false,
            settings_save: None,
            welcome_theme_dirty: false,
            splash_probe: None,
        }
    }

    /// 按当前版面 / 时刻决定要不要重建欢迎屏 header，要就重建。
    ///
    /// 每帧调用：海鸥动作 / 开屏扫光期间逐帧重建，定格且姿态没到点就一次不建 —— 动画
    /// header 不该每帧重新分配。三个门控信号：**版面**（`Frame`：内容宽 + 视口高 +
    /// transcript 空不空 —— 空会话时整块停在视口光学中心）、**干活**（`turn.working`，
    /// 站姿还是飞行）与**在视口里**（`ChatView::header_in_view`）。欢迎屏被滚出去之后
    /// 整条时钟停摆，常驻 idle 循环因此常态零成本。第四个信号是**会话事实**（skills /
    /// rules 计数，SyncSession 到达时变化）：名牌右列那一行随它重建。`welcome == None`
    /// （测试关掉了）时什么都不做。
    fn sync_welcome(&mut self, palette: &ThemePalette, view: Viewport, now: std::time::Instant) {
        let working = self.turn.working;
        let visible = self.chat.header_in_view();
        let facts = self.session_facts;
        // 调色板刚变过（实时预览）：`needs_rebuild` 的键里没有它，强制重建一次。
        let theme_dirty = std::mem::take(&mut self.welcome_theme_dirty);
        let Some(welcome) = self.welcome.as_mut() else {
            return;
        };
        let rebuild = if theme_dirty && visible {
            true
        } else {
            welcome.needs_rebuild(view, now, working, visible, facts)
        };
        if !rebuild {
            return;
        }
        let lines = welcome.build(palette, view, now, working, visible, facts);
        self.chat.set_header(lines);
    }

    /// 这一帧走不走开屏版式：`Some((牌子行数, 输入卡宽度))` = 走。
    ///
    /// 判据是**装得下**：`状态栏 + 牌子 + 间隔 + 输入卡 ≤ 屏高`（上方的空气可以被
    /// 压到 0，见 [`frame_layout`]）。牌子都没有（欢迎屏被关掉）时同样不走。
    ///
    /// 量牌子要按开屏宽度（终端整宽：开屏没有滚动条）建一次 header，所以结论按
    /// **内容宽**缓存 —— 矮终端上每帧都会退回会话版式，不缓存就会每帧把 header
    /// 重建两遍。缓存只存"高度"，装不装得下每帧用当前的输入卡高度重算（草稿
    /// 长大时判定要跟着变）。
    fn splash_plan(
        &mut self,
        palette: &ThemePalette,
        area: Rect,
        full_card_w: u16,
        now: std::time::Instant,
    ) -> Option<(u16, u16)> {
        if !self.chat.is_empty() || self.welcome.is_none() {
            return None;
        }
        let rows = match self.splash_probe {
            Some((width, rows)) if width == area.width => rows,
            _ => {
                self.sync_welcome(
                    palette,
                    Viewport {
                        width: area.width,
                        empty: true,
                    },
                    now,
                );
                let rows = self.chat.header_height() as u16;
                self.splash_probe = Some((area.width, rows));
                rows
            }
        };
        if rows == 0 {
            return None;
        }
        // 输入卡与牌子同宽居中（读起来是一块东西）。
        let composer_w = hero_block_width(self)
            .max(SPLASH_INPUT_MIN)
            .min(full_card_w);
        let fits = 1 + rows + 1 + self.input.height(composer_w) <= area.height;
        fits.then_some((rows, composer_w))
    }

    /// 抽中的那条 tip（测试断言首帧文案）。
    #[cfg(test)]
    pub(crate) fn tip_text(&self) -> &'static str {
        self.welcome
            .as_ref()
            .map(|w| w.tip().text)
            .unwrap_or_default()
    }

    /// 欢迎屏 header 建过多少次（测试：开屏判定不该让一帧建两遍）。
    #[cfg(test)]
    pub(crate) fn welcome_builds(&self) -> u32 {
        self.welcome.as_ref().map(Welcome::builds).unwrap_or(0)
    }

    /// 解析后的主题色板（测试要按同一份色板构建 header）。
    #[cfg(test)]
    pub(crate) fn palette(&self) -> ThemePalette {
        ThemePalette::from_config(&self.config.colors)
    }

    /// 彻底关掉欢迎屏（连 header 一起清空）。
    ///
    /// 测试要一块只有消息的干净顶部时用它 —— 只是 `chat.set_header(Vec::new())`
    /// 挡不住：下一次 draw 的 `sync_welcome` 会把 header 装回去。
    #[cfg(test)]
    pub(crate) fn clear_welcome(&mut self) {
        self.welcome = None;
        self.chat.set_header(Vec::new());
    }

    /// Whether the terminal is wide enough for detailed status bar.
    fn is_wide(&self) -> bool {
        self.geometry.width() >= WIDE_THRESHOLD
    }

    /// Mark the UI as changed (coalesced to ~60fps by the frame gate).
    /// Call after any side effect that may have mutated visible state
    /// outside the event handlers — intent execution, toasts, focus changes.
    fn mark_dirty(&mut self) {
        self.chat_dirty = true;
    }

    /// The image half of a frame boundary: workspace + store drain + metadata
    /// table refresh — see [`images::Images::sync`].
    ///
    /// Returns whether anything the frame depends on changed; a frame calls
    /// this twice — before the render (a result that landed is used by *this*
    /// frame) and after it (the candidates the render just discovered get
    /// probed, and the worker's answer is what wakes the loop).
    fn sync_images(&mut self) -> bool {
        if !self.images.is_enabled() {
            return false;
        }
        let (workdir, launch) = (
            self.status.workdir.as_deref(),
            self.launch_workspace.as_deref(),
        );
        if self.images.set_workspace(workdir.or(launch)) {
            // The cells' candidate lists were resolved against the *old* root:
            // feeding them now would put the paths we just dropped straight back
            // into the table. The render re-derives them (its options changed,
            // so every cell re-renders) and the frame's second pass feeds those.
            return true;
        }
        // A content rebuild (session switch / compaction / rewind) may name
        // files that changed on disk: re-read them rather than trusting the memo.
        self.images.set_structure_epoch(self.chat.structure_epoch());
        let candidates = self.chat.image_candidates();
        let changed = self.images.sync(candidates);
        // Media pictures: what the frame asked for and the lane does not have becomes one
        // background fetch (the intent is drained after this draw). Media objects are
        // immutable and session-scoped, so a batch that is still in flight is simply not
        // asked for again (`media_requests` owns that bookkeeping).
        let wanted = self.images.media_requests();
        if !wanted.is_empty() {
            self.push_intent(AppIntent::LoadMedia {
                session_id: self.session_id.clone(),
                media_ids: wanted,
            });
        }
        changed
    }

    /// The picture lane's freshness check — the event loop's half of
    /// [`images::Images::poll_freshness`].
    ///
    /// Called from the run loop when the lane says a check is due, **never** from [`App::draw`]: it
    /// `stat`s the pictures on screen, and a picture whose file changed drops the store's memo, which
    /// the next frame's `sync` turns into a fresh header probe and a new encoding. Disabled lanes (no
    /// protocol / `rendering.images: off`) short-circuit here, so "off" still reads no file. `now` is
    /// injected so the throttle can be tested without sleeping.
    fn poll_image_freshness(&mut self, now: std::time::Instant) -> bool {
        if !self.images.is_enabled() {
            return false;
        }
        self.images.poll_freshness(now)
    }

    /// Frame-gated draw decision.
    ///
    /// WS stream events (deltas, tool updates, …) only mark the chat
    /// dirty and are coalesced to [`MIN_FRAME_INTERVAL`] — the draw rate
    /// decouples from the delta event rate (3000 tokens/s ≈ 50 events/s
    /// would otherwise mean 50 full draws/s). User input, resize and full
    /// repaints bypass the gate and draw immediately.
    fn should_draw_now(&mut self) -> bool {
        let immediate = self.needs_full_redraw || self.input_dirty;
        if immediate {
            self.input_dirty = false;
            self.chat_dirty = false;
            return true;
        }
        if !self.chat_dirty {
            return false;
        }
        let elapsed = self.last_draw.elapsed();
        if draw_gate(elapsed) {
            self.chat_dirty = false;
            return true;
        }
        false
    }

    /// Common cleanup at the end of an agent turn (Done / Interrupted / Error): resets turn state,
    /// render context and copy candidates. Callers handle their own specific follow-up (title,
    /// toast, etc.).
    ///
    /// **Ordering note**: `refresh_copy_candidates()` runs *inside* this method, so any chat
    /// mutations by the caller (`clear_ask_state`, `chat.push(ErrorMessage)`) happen *after* the copy
    /// cache is snapshotted. Currently safe because `collect_assistant_messages` only collects
    /// `AssistantMessage` cells, which those mutations do not touch.
    fn finish_turn(&mut self) {
        self.turn.finish();
        self.ctx.reset();
        self.refresh_copy_candidates();
        // 回合结束：还在计时的思考块就地定格（`深度思考中 4s` → `深度思考 4s`）。
        self.chat.finish_active_thinking(std::time::Instant::now());
        // 同理，还没落结果的 Bash 卡直接丢弃计时器：完成时刻不可知，任何数值都会
        // 把「执行起点→回合结束」这个上界渲染成精确运行时长（#108 的假超时读法）。
        self.chat.discard_pending_bash_timers();
        // Turn-end reconcile: install the full reference render for all
        // streaming cells (converges any incremental drift, frees stream
        // state). The actual render happens at the next draw, where the
        // terminal width is known.
        self.chat.finalize_streams();
    }

    /// Workdir last-component label for the terminal title suffix (e.g. `myproject`).
    fn dir_label(&self) -> Option<String> {
        title::dir_label(self.status.workdir.as_deref())
    }

    /// Notify the user via OSC 9 + attention title when the terminal is not focused.
    fn notify_unfocused(&mut self, message: String, kind: AttentionKind) {
        if !self.focused {
            self.push_intent(AppIntent::Notify(message));
            self.push_intent(AppIntent::SetTitle(title::title_attention(
                kind,
                self.dir_label().as_deref(),
            )));
        }
    }

    /// Report the agent's state to the terminal via OSC 7501 (Program Status
    /// Protocol). The side-channel counterpart to the OSC 0 title: the same
    /// state, addressed to terminals and agent dashboards instead of the human
    /// in front of the tab. Idempotent reports are dropped by the reporter.
    fn set_program_status(&mut self, state: State, msg: Option<&str>) {
        if let Some(report) = self.program_status.report(state, msg) {
            self.push_intent(AppIntent::SetProgramStatus(report));
        }
    }

    /// Project the current state onto the program-status record: the front ask
    /// panel is what the agent is blocked on, otherwise the turn decides.
    /// Shared by the live Ask event, the answer path, and sync replay.
    fn sync_program_status(&mut self) {
        match self.ask_panels.front() {
            Some(panel) => {
                // The retired Bash confirmation is an approval gate; every
                // other interactive ask is a question. (`Notice` never enters
                // the queue — it is not interactive.)
                let kind = match panel.mode {
                    PanelMode::RequiredChoice => BlockKind::Permission,
                    PanelMode::Question | PanelMode::Notice => BlockKind::Question,
                };
                let msg = panel.notify_text();
                self.set_program_status(State::Blocked(kind), Some(&msg));
            }
            None => {
                let state = if self.turn.working {
                    State::Working
                } else {
                    State::Idle
                };
                self.set_program_status(state, None);
            }
        }
    }

    /// Show a toast notification. Returns remaining duration for timer setup.
    pub fn show_toast(&mut self, toast: Toast) -> std::time::Duration {
        let remaining = toast.remaining();
        self.toast = Some(toast);
        // A toast IS a visible change — always schedule a draw (the frame
        // gate coalesces bursts).
        self.chat_dirty = true;
        remaining
    }

    /// Clear the active toast (including persistent toasts).
    pub fn clear_toast(&mut self) {
        self.toast = None;
    }

    /// Push a side-effect intent for the runner to execute after draw.
    fn push_intent(&mut self, intent: AppIntent) {
        self.intents.push(intent);
    }

    /// Drain all pending intents. Called by the runner after each draw cycle.
    pub fn drain_intents(&mut self) -> Vec<AppIntent> {
        std::mem::take(&mut self.intents)
    }

    /// Update the gateway connection state.
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
        self.status.connected = connected;
    }

    /// Draw the UI.
    ///
    /// Generic over the backend so tests can drive it with a
    /// `ratatui::backend::TestBackend` and assert on the very frame the user
    /// would see. The selection highlight, its text snapshot and the
    /// scrollbar's clipping / place in the overlay order are all frame-level
    /// properties, not unit properties of the painters; production always
    /// passes the crossterm terminal.
    fn draw<B>(&mut self, terminal: &mut ratatui::Terminal<B>) -> Result<()>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        // Out-of-band terminal disturbance (focus regain / resize) — see
        // `needs_full_redraw`. Reset the back buffer so this draw repaints
        // the whole screen and resyncs with the terminal.
        if self.needs_full_redraw {
            terminal.clear()?;
            self.needs_full_redraw = false;
            // The same three triggers mean the terminal may have dropped the
            // pictures it was showing (a cleared screen, a re-laid-out window,
            // a tab the compositor forgot): drop every encoding and revoke the
            // handles, so the next request re-encodes and re-transmits.
            self.images.invalidate();
        }

        // Image half of the frame boundary, first pass: drain the worker and
        // refresh the metadata table *before* anything is laid out, so a probe
        // or encode that finished since the last frame is drawn in this one.
        self.sync_images();

        let mut chat_height: u16 = 0;

        // Extract render context before the draw closure borrows self.
        let palette = self.palette;
        let layout = self.config.layout.clone();
        let thinking_mode = self.config.rendering.thinking;
        let thinking_expanded = self.chat.thinking_expansion();
        // `tui::draw_frame` (not `Terminal::draw`): every frame this app puts on
        // the screen goes through the wire pass — see [`crate::ui::emoji_width`].
        crate::tui::draw_frame(terminal, |frame| {
            let area = frame.area();
            self.geometry.record_area(area);

            // A selection is anchored to *content* coordinates, which only
            // survive while the content is stable: adding / removing /
            // promoting cells or changing the width shifts the virtual rows
            // under a chat anchor, editing the draft moves the text under a
            // composer anchor. Abort instead of pointing the highlight at
            // different text. Streamed chat text growth does not (it never
            // moves an existing row), so it keeps the selection alive.
            if self
                .selection
                .needs_abort(|region| self.selection_fingerprint(region))
            {
                self.cancel_selection();
            }

            // Layout: status (1) | chat (fill) | composer block | [popup]. The composer is a fixed
            // block below the scrollable chat viewport, so it stays in view whatever the scroll
            // position. The card carries the frame, the draft and both rails (activity + meta): the
            // rows the working indicator and the info separator used to take are inside it, so a
            // running turn costs no row at all. The card *floats* inside its block (see
            // `chrome::card_area`), so the height is asked for in the columns the card will really
            // get — the request and the wrapping it produces have to describe the same frame.
            // 空会话 = 开屏：牌子先建一次（宽度 = 终端整宽，开屏没有滚动条），
            // 排版才知道它多高 —— 整块与输入卡要一起算光学中心。
            let now = std::time::Instant::now();
            let full_card_w = crate::ui::input_area::chrome::card_area(area).width;
            let splash_plan = self.splash_plan(&palette, area, full_card_w, now);
            let (splash, hero_rows) = match splash_plan {
                Some((hero_rows, _)) => (true, hero_rows),
                None => (false, 0),
            };
            let composer_w = splash_plan.map_or(full_card_w, |(_, width)| width);
            let composer_h = self.input.height(composer_w);
            let bands = frame_layout(area, splash, hero_rows, composer_h, self.popup.height());

            // Status bar (model top-left, cumulative usage, connection) — plus
            // the session identity tail (id + pin star), the two pointer
            // targets the bar itself declares. The regions the widget really
            // drew are recorded here, next to the other frame geometry: a
            // click between frames must hit what the user was looking at.
            let mut status_regions = crate::ui::status_bar::StatusBarRegions::default();
            frame.render_widget(
                StatusBar::new(
                    &self.status,
                    &self.session_id,
                    self.is_wide(),
                    &palette,
                    &mut status_regions,
                ),
                bands.status,
            );
            self.geometry.record_status_regions(status_regions);

            // Chat view — the scrollable viewport only. The widget is rendered
            // into the band **minus the scrollbar gutter**, so no cell
            // (markdown text, user-message background, diff tint, tool output)
            // can reach the bar's column however its own width arithmetic
            // works out. The recorded chat band keeps the full width: that is
            // what the bar's geometry and its hit testing are derived from.
            chat_height = bands.band.height;
            self.geometry.record_chat_band(bands.band);

            // 欢迎屏 header：版面（宽度档 / 视口高 / 空会话）、姿态、扫光变了就
            // 重建一次 —— 在 chat widget 之前完成，这一帧画的就是新行。宽度按
            // **内容宽**算：header 与消息渲染在同一个（减掉滚动条 gutter 的）
            // 矩形里，按终端整宽构建会让最右一列连同省略号被裁掉。
            // 开屏的 band 就是牌子本身（没有 gutter：没有内容可滚），会话里才
            // 减掉滚动条 gutter —— header 与消息渲染在同一个矩形里。
            let chat_area = if splash {
                bands.band
            } else {
                scrollbar::content_area(bands.band)
            };
            self.sync_welcome(
                &palette,
                Viewport {
                    width: chat_area.width,
                    empty: splash,
                },
                now,
            );
            let ctx = crate::render::renderable::CellContext {
                palette: &palette,
                thinking_mode,
                thinking_expanded,
                layout: &layout,
                images: self.images.opts(),
            };
            frame.render_widget(ChatViewWidget::new(&mut self.chat, ctx), chat_area);

            // Overlay scrollbar. Painted after the chat widget (so it overprints the gutter's
            // blank columns) and before the toast (so a toast is never hidden by it). It takes no
            // layout width: the gutter is reserved unconditionally, so the bar appearing on overflow
            // never reflows the cells. `self.chat` now holds this frame's content height and the
            // effective scroll offset (auto-scroll / clamp included).
            //
            // 设置面板开着时不画：overlay 是最后写入者，画在它下面只是白费；顺手清掉 hover / drag
            // 态，免得关掉面板后拖拽态复活。
            if self.settings_panel.is_some() {
                self.clear_scrollbar_interaction();
            } else if let Some(geom) = self.scrollbar_geometry() {
                scrollbar::paint(frame.buffer_mut(), &geom, self.scrollbar, &palette);
            } else {
                // No bar this frame (content fits, or nothing drawn yet):
                // drop the interaction state now instead of waiting for the
                // next mouse event, so a later overflow cannot resurrect a
                // stale hover / drag look.
                self.clear_scrollbar_interaction();
            }

            // The composer card — always visible; the rect it is laid out into
            // is recorded in the frame's geometry (the widget keeps its own
            // copy for cursor placement): mouse events arrive between frames,
            // so hit testing works off the last frame's rect — the same
            // contract as the chat band's. The two rails carry what used to be
            // rows of their own: what the agent is doing (top) and where the
            // session stands — workdir, this turn's usage, scroll position
            // (bottom).
            let activity = if self.turn.working {
                self.turn.started_at.map(|started_at| ActivityRail {
                    spinner: &self.turn.spinner,
                    started_at,
                })
            } else {
                None
            };
            let meta = MetaRail {
                workdir: self.status.workdir.as_deref(),
                usage: &self.turn.usage,
                total_lines: self.chat.content_height(),
                visible_height: chat_height as usize,
                scroll_offset: self.chat.scroll_position(),
            };
            let input_rect = if splash {
                centered_card(bands.composer, composer_w)
            } else {
                crate::ui::input_area::chrome::card_area(bands.composer)
            };
            let popup_rect = if splash {
                splash_popup(input_rect, self.popup.height(), area)
            } else {
                bands.popup
            };
            let keyboard_held = self.composer_typing_blocked();
            self.geometry.record_composer(input_rect);
            frame.render_widget(
                ComposerWidget::new(&mut self.input, &palette, activity, meta)
                    .keyboard_held(keyboard_held),
                input_rect,
            );

            // Pop-down command popup — below the composer and floated with it,
            // so the list hangs off the card instead of the screen edge.
            if let Some(rect) = popup_rect
                && let Some((rows, state, filter)) = self.popup.active.render_data()
            {
                // 开屏的 `rect` 已经是卡片矩形（与输入卡同宽同列），再剥一层边距
                // 会比卡片窄 2 列；会话里的 `rect` 是排版行，才要按惯例留边。
                let area = if splash {
                    rect
                } else {
                    crate::ui::input_area::chrome::card_area(rect)
                };
                frame.render_widget(SelectionPopup::new(rows, state, filter, &palette), area);
            }

            // Position the input cursor *inside* the render pass so ratatui
            // owns cursor show/hide/move. The previous post-draw
            // `execute!(Show/Hide, MoveTo)` wrote to the backend out-of-band,
            // which desyncs ratatui's cursor tracking and is explicitly
            // discouraged by ratatui.
            //
            // 设置面板开着时不定位：面板自己画反显光标，终端光标留在 overlay 上
            // 就是一个游离的方块（不调用它 = 这一帧隐藏光标）。
            if self.settings_panel.is_none() {
                let (cursor_x, cursor_y) = cursor_screen_pos(&self.input, &input_rect);
                let cursor_x = cursor_x.min(area.width.saturating_sub(1));
                let cursor_y = cursor_y.min(area.height.saturating_sub(1));
                frame.set_cursor_position((cursor_x, cursor_y));
            }

            // 设置面板：居中**浮层卡片**（v2），垫在 toast *之前*（保存回执的 toast 要压在
            // 它上面）。卡片自己 `Clear` 自己那块——四周的聊天背景因此还在（v1 是全屏
            // overlay，这里额外全屏 `Clear` 一次；浮层不再需要，也不该抹掉背景）。
            let settings_card = self
                .settings_panel
                .as_ref()
                .map(|_| crate::ui::settings::card_area(area));
            if let Some(panel) = self.settings_panel.as_mut()
                && let Some(card) = settings_card
            {
                // 契约：每帧把两栏各自的可见行数告诉面板（翻页步长）。
                panel.set_viewport_rows(
                    crate::ui::settings::tree_viewport_rows(panel, card) as usize
                );
                panel.set_anchor_viewport_rows(crate::ui::settings::anchors_viewport_rows(
                    panel, card,
                ) as usize);
                panel.set_anchors_visible(crate::ui::settings::has_anchor_column(panel, card));
            }
            if let Some(panel) = self.settings_panel.as_ref()
                && let Some(card) = settings_card
                && let Some((schema, _)) = self.settings_cache.as_ref()
            {
                crate::ui::settings::SettingsOverlay::new(
                    panel,
                    crate::ui::settings::SettingsCatalogs::new(
                        &schema.root,
                        self.interface_catalog.as_ref(),
                    ),
                    &palette,
                )
                .render(card, frame.buffer_mut());
                // 卡片盖住的链接 hit box 要作废（与 toast 同一条不变量：看不见的
                // hit box 不许打开看不见的链接）。指针侧另有一道门
                // （`background_pointer_blocked`），这里保证的是**表本身**不撒谎。
                self.chat.mask_links(card);
            }

            // Toast overlay (rendered last, on top of everything).
            let mut toast_area: Option<Rect> = None;
            if let Some(ref toast) = self.toast
                && !toast.is_expired()
            {
                toast_area = render_toast(toast, area, frame.buffer_mut(), &palette);
                // The toast paints over the chat band (top-right, below the
                // status bar): its cells are gone, so the link hit boxes under
                // it must go too.
                if let Some(area) = toast_area {
                    self.chat.mask_links(area);
                }
            }

            // Markdown pictures — the last write of the frame (see `ui::image::paint`): every widget
            // above has already painted, so nothing can land on a placeholder or a sixel anchor
            // afterwards. Overlays are respected by *not* drawing: a picture whose box the toast would
            // cover is skipped whole (a partial overdraw would break the image), and a drag selection
            // keeps the frame text-only — its snapshot copies the visible rows and the row content must
            // not change under the finger.
            //
            // The frame's placement table is recorded before those gates: it is also the freshness
            // lane's target set, and a picture on screen but suppressed *this* frame is still worth
            // keeping in step with its file (see `Images::observe_visible`).
            let clip = self.chat.geometry().area;
            let recorded = self.chat.frame_images();
            self.images.observe_visible(recorded);
            // 浮层卡片是"挡一块"而不是"挡全屏"：背景里的图片照常画，被卡片（或 toast）
            // 压住的那几张由 `paint` 的 masks **整张**跳过（半张覆盖会撕碎图形协议）。
            // 关面板时 `close_settings_panel` 会 invalidate
            // 一次，编码与句柄在那里作废。
            let mut masks: Vec<Rect> = toast_area.into_iter().collect();
            masks.extend(settings_card);
            if !self.selection.is_press_active() {
                self.images
                    .paint(recorded, clip, &masks, frame.buffer_mut());
            }

            // In-app text selection — painted after the toast (the selection sits above every
            // overlay) and clipped to *this* frame's rect of the region that owns it. It is a pure
            // `Buffer` patch: merging `REVERSED` into the cell styles the widgets just produced, so no
            // cell / widget code has to know about selections and the background colors survive.
            //
            // The chat pass also snapshots the visible rows: the release event lands between frames,
            // so the copy must come from the frame the user was actually looking at. The composer
            // needs no snapshot — its wrapping is ours, so the copy comes from the draft itself.
            //
            // 设置面板开着时不画：面板是**模态浮层**，键盘已经被它接管、指针也不再认领背景
            // （`background_pointer_blocked`），而拖拽坐标落在卡片四周的 chat band 上（选区锚在
            // 内容坐标）—— 高亮画出来只会和卡片打架（打开面板时已有的选区已被取消，这里挡的是
            // "面板开着时新起的拖拽"）。
            if self.selection.is_press_active() && self.settings_panel.is_none() {
                match self.selection.region() {
                    Some(SelectionRegion::Chat) => {
                        self.chat
                            .paint_selection(frame.buffer_mut(), self.selection.state());
                        self.chat.capture_visible_rows(frame.buffer_mut());
                    }
                    Some(SelectionRegion::Composer) => {
                        pointer::paint_selection(
                            frame.buffer_mut(),
                            &self.input,
                            input_rect,
                            self.selection.state(),
                        );
                    }
                    None => {}
                }
            }
        })?;

        // Lazy cleanup expired toast after draw closure.
        if self.toast.as_ref().is_some_and(|t| t.is_expired()) {
            self.toast = None;
        }

        // Image half of the frame boundary, second pass: the cells just
        // rendered discovered their candidates, so probe anything new here —
        // the worker answering is what wakes the loop (`ImageStoreConfig::waker`).
        // Anything that landed this late makes the frame that was just drawn
        // stale, so ask for another one.
        if self.sync_images() {
            self.mark_dirty();
        }

        Ok(())
    }
}

/// Minimum spacing between chat-dirty draws (≈60fps frame budget).
const MIN_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Timer arm of the run loop's `select!` for the welcome block's opening sweep.
///
/// Same absolute-deadline contract as the selection lane's
/// [`selection_autoscroll_tick`]: `select!` rebuilds the future on every loop
/// iteration, so a relative sleep would be pushed back by every key / stream
/// event and the sweep would freeze mid-flight. `None` (swept and settled)
/// parks in `pending()` — a settled welcome costs the loop nothing.
async fn welcome_sweep_tick(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending::<()>().await,
    }
}

/// Timer arm of the run loop's `select!` for the thinking label's shimmer.
///
/// 与欢迎屏扫光同一条契约：**绝对**截止时刻（网格锚在块的起点上，见
/// `ThinkingBlock::next_frame`），否则流式事件会把"睡 40ms"无限推后、光带卡死；
/// `None`（没有活跃块 / 块不在视口）park —— 看不见的动画不花钱。
async fn thinking_sweep_tick(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending::<()>().await,
    }
}

/// 牌子下面那几行的**基线**高度：一格间隔 + 单行输入卡（上下各一道边框）+ 一格。
///
/// 光学中心按它算（弹层与草稿长大不进这个账），见 `welcome::splash_pad`。
const SPLASH_RESERVE: u16 = 1 + crate::ui::input_area::chrome::BORDER_ROWS + 1;

/// 开屏输入卡的最小宽度：牌子在窄终端上只有 38 列左右（Compact 档），而
/// composer 的两条轨道（workdir / 用量）比它更需要地方 —— 取一个下限，
/// 读起来仍是"牌子下面一张卡"，不是一根细条。
const SPLASH_INPUT_MIN: u16 = 48;

/// 这一帧的版面：状态栏 / chat band / composer 块 / 弹层。
///
/// 两种版式，差别只在**空会话**：
///
/// * **会话里**：`status(1) | band(fill) | composer | [popup]` —— 输入卡钉在底部，
///   band 吃掉剩下的全部高度（历史可滚）。
/// * **开屏**（`hero_rows > 0`）：`status | 空气 | 牌子 | 间隔 | 输入卡 | [弹层] | 空气`
///   —— 牌子与输入卡作为**一块**停在光学中心（[`splash_pad`]），下面那片空气吸收
///   弹层与草稿长大：用户敲 `/` 打开命令列表时，牌子不动。
struct FrameLayout {
    status: Rect,
    band: Rect,
    /// composer 的**版块**（widget 会自己在这块里留边距，见 `chrome::card_area`）。
    composer: Rect,
    /// 命令弹层（挂在输入卡下面）；不显示时为 `None`。
    popup: Option<Rect>,
}

/// 排这一帧的版面（见 [`FrameLayout`]）。`hero_rows == 0` = 会话版式。
fn frame_layout(
    area: Rect,
    splash: bool,
    hero_rows: u16,
    composer_h: u16,
    popup_h: u16,
) -> FrameLayout {
    if !splash {
        let mut constraints = vec![
            Constraint::Length(1),          // status bar
            Constraint::Min(3),             // chat view (min 3 rows)
            Constraint::Length(composer_h), // composer card
        ];
        if popup_h > 0 {
            constraints.push(Constraint::Length(popup_h)); // pop-down command popup
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        return FrameLayout {
            status: chunks[0],
            band: chunks[1],
            composer: chunks[2],
            popup: (popup_h > 0).then(|| chunks[3]),
        };
    }

    // 开屏的空气按**基线**算（单行输入卡 + 间隔）：弹层与草稿长大只吃下面的空气，
    // 不吃上面那份 —— 否则牌子会跟着往上跳。装不下时先把空气压到 0，**绝不让约束
    // 总和超出屏高**：超了 ratatui 会去挤牌子（`Length` 也能被压），名牌最后一行
    // 被整行裁掉、band 还会变得比 header 矮（顺手画出一条滚动条）。
    let pad = crate::ui::welcome::splash_pad(area.height, hero_rows, SPLASH_RESERVE)
        .min(area.height.saturating_sub(1 + hero_rows + 1 + composer_h));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // status bar
            Constraint::Length(pad),        // 上方空气
            Constraint::Length(hero_rows),  // 牌子（= chat band，只有 header）
            Constraint::Length(1),          // 牌子与输入卡之间的呼吸
            Constraint::Length(composer_h), // 输入卡
            Constraint::Fill(1),            // 下方空气
        ])
        .split(area);

    // 开屏的弹层是**浮层**，位置由 draw 按输入卡的真实矩形算（见
    // [`splash_popup`]）：它不占排版空间，所以不推动任何东西。
    let _ = popup_h;
    FrameLayout {
        status: chunks[0],
        band: chunks[2],
        composer: chunks[4],
        popup: None,
    }
}

/// 开屏的弹层：挂在输入卡下面（**同宽同列**）、盖住空气，不占排版空间。
///
/// 不占排版的理由有两条：一是它不推动任何东西（否则用户一按 `/` 整块就往上跳）；
/// 二是开屏没有可滚的历史需要让位。屏幕太矮时它被屏幕底边裁掉（列表本身有窗口，
/// 上下键翻）。
fn splash_popup(input: Rect, popup_h: u16, screen: Rect) -> Option<Rect> {
    let room = screen.bottom().saturating_sub(input.bottom());
    (popup_h > 0 && room > 0).then(|| Rect {
        x: input.x,
        y: input.bottom(),
        width: input.width,
        height: popup_h.min(room),
    })
}

/// 一块牌子的宽度：上一帧 title 行的显示宽度（不含居中的空列）。
fn hero_block_width(app: &App) -> u16 {
    app.welcome
        .as_ref()
        .map(|welcome| welcome.block_width())
        .unwrap_or(0)
}

/// 把输入卡按 `width` 居中放进 `block`（先按 composer 的惯例留边距）。
fn centered_card(block: Rect, width: u16) -> Rect {
    let area = crate::ui::input_area::chrome::card_area(block);
    let width = width.min(area.width);
    Rect {
        x: area.x + (area.width - width) / 2,
        width,
        ..area
    }
}

/// Pure frame-gate predicate: a chat-dirty draw is due iff at least
/// [`MIN_FRAME_INTERVAL`] has elapsed since the last draw.
fn draw_gate(elapsed_since_last_draw: std::time::Duration) -> bool {
    elapsed_since_last_draw >= MIN_FRAME_INTERVAL
}

/// Timer arm of the run loop's `select!` for the picture freshness check.
///
/// Same absolute-deadline contract as the other timer arms: `select!` rebuilds
/// the future on every loop iteration, so a relative sleep would be pushed back
/// by every key press and stream event — the deadline has to come from the
/// lane's own clock (`last_check + FRESHNESS_INTERVAL`). `None` (nothing
/// anchored, or no pictures at all) parks, so this lane costs an idle session
/// nothing.
async fn image_freshness_tick(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending::<()>().await,
    }
}

/// TUI 退出时携带出的状态——终端恢复后打印的退出摘要素材。
///
/// 只带**本地**事实（会话 id / 模型 / 墙钟 / 状态栏累计）：服务端账本由
/// `cmd::exit_summary::read_ledger` 在终端恢复**之后**另取（不响应的网关
/// 不该让用户对着备用屏等），失败时正是这里的累计值兜底。
pub struct TuiExit {
    pub session_id: String,
    /// 展示用模型名（display name ‖ 调用名；与状态栏同一取法）。
    pub model: String,
    /// 进入 TUI → 退出（run_app 本地计时，与网关时钟无关）。
    pub wall: std::time::Duration,
    /// 状态栏累计（快照基线 + live 增量）——摘要降级时的素材。
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
}

impl TuiExit {
    /// 从退出时刻的 App 取本地素材（服务端账本由 TUI 入口在终端恢复后另取）。
    pub(crate) fn from_app(app: &App, wall: std::time::Duration) -> Self {
        Self {
            session_id: app.session_id.clone(),
            model: app.status.model_label().to_string(),
            wall,
            prompt_tokens: app.status.session_prompt_tokens,
            completion_tokens: app.status.session_completion_tokens,
            cached_tokens: app.status.session_cached_tokens,
        }
    }
}

/// Run the main application loop.
pub async fn run_app(
    terminal: &mut WingTerminal,
    transport: Transport,
    session_id: String,
    mut endpoint: GatewayEndpoint,
    config: AppConfig,
    launch_workspace: Option<String>,
) -> Result<TuiExit> {
    let entered = std::time::Instant::now();
    // Terminal graphics: probe **once**, before `spawn_event_stream` — the
    // query writes to stdout and reads stdin, so starting the event reader
    // first would let the two race for the same file descriptors. `off` skips
    // the probe entirely (no query, no tmux side effects, no startup delay).
    let image_support = match config.rendering.images {
        crate::config::rendering::ImagesMode::Off => crate::ui::image::ImageSupport::disabled(),
        crate::config::rendering::ImagesMode::Auto => {
            crate::ui::image::ImageSupport::detect(crate::ui::image::DEFAULT_DETECT_TIMEOUT)
        }
    };
    // The worker's wake-up: a `Notify` (Thread-safe, no reactor needed) poked
    // from the store's worker thread as soon as a probe or an encode finishes.
    // The arm below turns it into a normal dirty draw.
    let image_wake = Arc::new(tokio::sync::Notify::new());
    let image_waker: Option<Arc<dyn Fn() + Send + Sync>> = {
        let wake = Arc::clone(&image_wake);
        Some(Arc::new(move || wake.notify_one()))
    };
    let images = Images::new(config.rendering.images, image_support, image_waker);

    // stdin 接驳的探针判定（查询在 `begin_tui_session` 里随 install 发出）：必须收束在
    // `spawn_event_stream` 之前——那是本进程第一次 crossterm poll 的创建点，事件源注册之后
    // fd 0 就不能再换，阴性拆接驳只有这一次机会。等待预算与上面的图像能力探测重叠，
    // 正常路径到这里应答早已到达、不再花时间；最坏也只是同步阻塞当前 worker 一次预算
    // （与上面的图像探测同款取舍）。
    let _ = crate::tui::stdin_proxy::settle_paste_channel();

    let mut app = App::with_images(session_id, config, launch_workspace, images);
    // Initial program-status record: the session starts at rest (a mid-turn
    // resume replaces this with `working` when its SyncSession lands).
    app.set_program_status(State::Idle, None);
    let mut term_events = crate::tui::spawn_event_stream();
    let mut transport = Some(transport);
    // Session recovery pending: the transport (WS) is up but the session
    // event subscription isn't re-established yet. Cleared once
    // `recover_session` succeeds.
    let mut recovery_pending = false;
    let mut retry_attempt: u32 = 0;
    let mut retry_at = std::time::Instant::now();

    // Channel for background fetch results.
    let (fetch_tx, mut fetch_rx) =
        tokio::sync::mpsc::channel::<crate::app::intent::FetchResult>(32);

    // Message-send pipeline tail: `SendMessage` runs in a background task, and
    // each new send waits for the previous one so frames keep submission order
    // (see `runner::spawn_send_message`). The loop never awaits the chain.
    let mut send_chain: Option<tokio::task::JoinHandle<()>> = None;

    // Request system info on startup via HTTP intents.
    app.push_intent(AppIntent::FetchInfo);
    app.push_intent(AppIntent::FetchCommands);

    // Set initial terminal title (workdir suffix appears once FetchInfo lands).
    {
        let writer = terminal.backend_mut();
        let _ = title::set_title(writer, &title::title_idle(app.dir_label().as_deref()));
    }

    loop {
        // Draw — frame-gated: streaming deltas coalesce, input draws
        // immediately, idle iterations skip the draw entirely.
        if app.should_draw_now() {
            if let Err(e) = app.draw(terminal) {
                tracing::error!("draw error: {e}");
            }
            app.last_draw = std::time::Instant::now();
        }

        // Execute all pending intents produced during the last event cycle.
        // Intents may mutate visible state (session switch + replay, popup
        // status, toasts on failure) — mark dirty uniformly instead of
        // relying on each intent remembering to.
        for intent in app.drain_intents() {
            runner::execute_intent(
                &mut app,
                &mut transport,
                &mut endpoint,
                terminal,
                intent,
                &fetch_tx,
                &mut send_chain,
            )
            .await;
            app.mark_dirty();
        }

        if app.should_quit {
            break;
        }

        // Compute toast expiry sleep for the select below.
        let toast_remaining = app
            .toast
            .as_ref()
            .filter(|t| !t.is_expired())
            .map(|t| t.remaining());

        // Retry sleep (fires when disconnected or session recovery is pending).
        let need_retry = transport.is_none() || recovery_pending;
        let retry_sleep = async {
            if need_retry {
                let now = std::time::Instant::now();
                if retry_at > now {
                    tokio::time::sleep(retry_at - now).await;
                }
            } else {
                std::future::pending::<()>().await;
            }
        };

        // Select from term events and gateway events.
        tokio::select! {
            Some(term_event) = term_events.recv() => {
                match term_event {
                    TermEvent::Key(key) => {
                        app.handle_key(key);
                        app.input_dirty = true;
                    }
                    TermEvent::Mouse(mouse) => {
                        // Discrete gestures (a wheel notch, a press, a
                        // release) are direct user actions: draw right away,
                        // bypassing the 16 ms frame gate, like keys do.
                        // Pointer motion and drags arrive in bursts (~100 Hz
                        // on a trackpad) and only mark the chat dirty, so the
                        // frame gate coalesces them. Events that change
                        // nothing (a hover that stays put, a drag that lands
                        // on the same position) ask for no redraw at all — so
                        // their floods never force one.
                        match app.handle_mouse(mouse) {
                            MouseOutcome::Ignored => {}
                            MouseOutcome::Coalesced => app.chat_dirty = true,
                            MouseOutcome::Immediate => app.input_dirty = true,
                        }
                    }
                    TermEvent::Paste(text) => {
                        app.handle_paste(&text);
                        app.input_dirty = true;
                    }
                    TermEvent::Clipboard(event) => {
                        // 进度只是「同一张 chip 换了个数字」：让它走帧闸门（合并重绘，
                        // 传输中最多 10 次/秒），不要每条都逼一帧。开始 / 交付 / 失败
                        // 都是离散的用户可见状态，立即画。
                        let coalesced = matches!(event, crate::tui::ClipboardEvent::ReadProgress { .. });
                        app.handle_clipboard(event);
                        if coalesced {
                            app.chat_dirty = true;
                        } else {
                            app.input_dirty = true;
                        }
                    }
                    TermEvent::Resize(_, _) => {
                        // ratatui auto-resizes its buffers on the next draw;
                        // force a full repaint so the resized screen is fully
                        // resynced with the terminal.
                        app.needs_full_redraw = true;
                    }
                    TermEvent::Focus(focused) => {
                        app.focused = focused;
                        app.mark_dirty();
                        // A drag that leaves the window never reports its
                        // release — drop the in-flight selection so no
                        // highlight (and no frozen follow state) is left
                        // behind.
                        if !focused {
                            app.cancel_selection();
                        }
                        // On focus regain, restore the correct title.
                        if focused {
                            // The terminal re-shows its surface on focus
                            // regain and may have drifted from ratatui's
                            // buffer (observed: stale glyphs on the status
                            // bar after switching tabs). Force a full repaint.
                            app.needs_full_redraw = true;
                            let dir = app.dir_label();
                            let title = if app.turn.working {
                                title::title_working(
                                    app.turn.spinner.frame_str(),
                                    dir.as_deref(),
                                )
                            } else {
                                title::title_idle(dir.as_deref())
                            };
                            app.push_intent(AppIntent::SetTitle(title));
                        } else {
                            // The button-up of an in-flight scrollbar drag may
                            // be delivered to whatever window took the focus —
                            // drop the interaction so the bar does not stay
                            // stuck in its dragged look (and so stray drag
                            // events stop moving the view).
                            app.clear_scrollbar_interaction();
                        }
                    }
                    TermEvent::Tick => {
                        let now = std::time::Instant::now();
                        let dt = now.duration_since(app.last_tick);
                        app.last_tick = now;
                        // 在途图片的 spinner（与 turn 的 spinner 同一个节拍）：有在飞的
                        // 占位符才动，动过才重绘 —— 空闲草稿一次活都不干。
                        if app.input.tick_pending_images(dt) {
                            app.chat_dirty = true;
                        }
                        if app.turn.working {
                            app.turn.spinner.tick(dt);
                            // Update title only when spinner frame changed.
                            let new_title = title::title_working(
                                app.turn.spinner.frame_str(),
                                app.dir_label().as_deref(),
                            );
                            if app.turn.last_title.as_deref() != Some(&new_title) {
                                app.turn.last_title = Some(new_title.clone());
                                app.push_intent(AppIntent::SetTitle(new_title));
                            }
                            // Refresh Bash tool timers. Only meaningful while a
                            // turn is active (a tool can only be pending mid-turn);
                            // gating here keeps idle sessions from scanning cells.
                            app.chat.tick_bash_timers();
                            // Spinner animation / tool timers changed the UI —
                            // ONLY while a turn is active. An idle session's
                            // ticks draw nothing (idle iterations skip
                            // drawing entirely); toasts are static (their
                            // expiry has its own timer arm).
                            app.chat_dirty = true;
                        }
                    }
                }
            }
            event = async {
                transport.as_mut().unwrap().ws.recv_event().await
            }, if transport.is_some() => {
                match event {
                    Some(e) => {
                        app.handle_event(e);
                        // Any gateway event may have mutated chat state;
                        // coalesced by the frame gate.
                        app.chat_dirty = true;
                    }
                    None => {
                        // Disconnected. Carry the read task's reason into the
                        // toast so the user can tell "gateway restarted" from
                        // "this session's payload exceeds the frame limit".
                        let reason = transport
                            .as_ref()
                            .and_then(|t| t.ws.close_reason())
                            .map(|r| crate::util::osc9::truncate_bytes(&r.describe(), 120));
                        transport = None;
                        app.set_connected(false);
                        let text = match reason {
                            Some(reason) => format!("⚡ Connection lost: {reason} — reconnecting..."),
                            None => "⚡ Connection lost — reconnecting...".to_string(),
                        };
                        app.show_toast(Toast::persistent(text, ToastKind::Warning));
                        app.chat_dirty = true;
                        retry_attempt = 0;
                        retry_at = std::time::Instant::now() + backoff(0);
                    }
                }
            }
            // Retry timer: reconnect transport first, then recover the
            // session over it (two decoupled phases — session failures never
            // dispose of the WebSocket).
            _ = retry_sleep => {
                if transport.is_none() {
                    // Phase 1: transport-level reconnect. One WebSocket for
                    // everything that follows, including phase-2 retries.
                    match connect_transport(&endpoint).await {
                        Ok(new_transport) => {
                            transport = Some(new_transport);
                            recovery_pending = true;
                            retry_attempt = 0;
                            // Try session recovery immediately.
                            retry_at = std::time::Instant::now();
                        }
                        Err(e) => {
                            retry_attempt += 1;
                            retry_at = std::time::Instant::now() + backoff(retry_attempt);
                            tracing::warn!(
                                "reconnect attempt {retry_attempt} failed: {e:#}"
                            );
                        }
                    }
                } else if let Some(t) = transport.as_ref() {
                    // Phase 2: session recovery over the existing WebSocket.
                    // A resume 404 (session never persisted / deleted) makes
                    // this silently start a fresh session — like first launch.
                    // The pushed SyncSession updates App state (session_id,
                    // chat replay), exactly like the `/new` command.
                    let workspace = app.launch_workspace.clone();
                    let lost_session = app.session_id.clone();
                    match t.recover_session(&app.session_id, workspace.as_deref()).await {
                        Ok(recovery) => {
                            recovery_pending = false;
                            app.set_connected(true);
                            app.clear_toast();
                            // 归档会话：新会话可用，但老会话要用户自己恢复——
                            // 提示由 `Recovery::notice` 给出（带可复制的命令）。
                            app.show_toast(recovery.notice(&lost_session).unwrap_or_else(|| {
                                Toast::info("Reconnected!", std::time::Duration::from_secs(2))
                            }));
                            // The session may have been silently replaced —
                            // the popup list must re-fetch.
                            app.invalidate_session_cache();

                            // Re-request info and commands via HTTP intents.
                            app.push_intent(AppIntent::FetchInfo);
                            app.push_intent(AppIntent::FetchCommands);
                        }
                        Err(e) => {
                            retry_attempt += 1;
                            retry_at = std::time::Instant::now() + backoff(retry_attempt);
                            tracing::warn!(
                                "session recovery attempt {retry_attempt} failed: {e:#}"
                            );
                        }
                    }
                }
            }
            // Toast expiry: trigger a redraw to clear the toast overlay.
            _ = async {
                if let Some(d) = toast_remaining {
                    tokio::time::sleep(d).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                // Toast expired — next draw() will lazy-cleanup.
                app.chat_dirty = true;
            }
            // Selection edge auto-scroll: one line per *absolute* deadline
            // (see `selection_autoscroll_tick` — a relative sleep would be
            // restarted by every incoming event and starve during streaming).
            // The arm carries no state of its own: the deadline is cleared the
            // moment the selection ends or hits the content edge, so nothing is
            // left spinning (and no busy loop when the view cannot move).
            _ = selection_autoscroll_tick(app.selection.deadline()) => {
                if app.tick_selection_autoscroll() {
                    app.input_dirty = true;
                }
            }
            // Welcome: repaint the header at the animation's own cadence (the
            // 100 ms system tick is too coarse). While the block is in view the
            // arm keeps waking — the opening sweep ticks per frame (~25 fps),
            // then the idle loop wakes on each action deadline (measured
            // ~2.4/s, and ~99% of those change the pose, i.e. cost one repaint
            // of the ~15 header lines). It parks only when the header scrolls
            // out of view (next_frame returns None). chat_dirty (not
            // input_dirty): the frame gate may coalesce it, and no key is
            // waiting on this frame.
            _ = welcome_sweep_tick(app.welcome.as_ref().and_then(|w| {
                w.next_frame(
                    std::time::Instant::now(),
                    app.turn.working,
                    app.chat.header_in_view(),
                )
            })) => {
                app.chat_dirty = true;
            }
            // Thinking label: 刷光帧驱动（≈25fps）。推进相位 / 秒数并把那一个
            // cell 作废；块冻结、滚出视口或没有任何活跃块时这条臂 park。
            _ = thinking_sweep_tick(app.chat.reasoning_sweep_deadline(std::time::Instant::now())) => {
                if app.chat.tick_active_thinking(std::time::Instant::now()) {
                    app.chat_dirty = true;
                }
            }
            // Background fetch results (non-blocking HTTP queries).
            Some(result) = fetch_rx.recv() => {
                app.handle_fetch_result(result);
                app.chat_dirty = true;
            }
            // Picture freshness: re-`stat` the pictures on screen at the lane's
            // own cadence and drop the store's memo for the ones whose file
            // changed (see `Images::poll_freshness`). This arm is why a
            // rewrite is noticed even in a session that draws no frames — the
            // model's last tool call ends, the loop goes idle, and the check
            // still runs. Nothing changed → no repaint: the frame is only
            // marked dirty when a file really moved.
            _ = image_freshness_tick(app.images.freshness_deadline(std::time::Instant::now())) => {
                if app.poll_image_freshness(std::time::Instant::now()) {
                    app.mark_dirty();
                }
            }
            // The image worker finished something (a header probe, an encode):
            // the next draw polls it into place. `chat_dirty` rather than
            // `input_dirty` — this is content, and the 16 ms frame gate may
            // coalesce it; the draw itself polls, so a permit consumed early
            // cannot lose the result (it is in the store's channel).
            _ = image_wake.notified() => {
                app.chat_dirty = true;
            }
            else => {
                break;
            }
        }
    }

    // Restore the terminal title on exit. The program-status record is left
    // alone on purpose: the spec's lifetime rules already do the right thing
    // when the process exits — the terminal drops `working` / `blocked`, and
    // `done` / `error` survive so the user still finds them. An exit `clear`
    // would only delete records that are meant to be kept.
    {
        let writer = terminal.backend_mut();
        let _ = title::set_title(writer, "");
    }

    // Exit material for the summary the (already restored) terminal prints:
    // local facts only — the server ledger is read by the caller afterwards,
    // so an unresponsive gateway cannot freeze the alternate screen.
    Ok(TuiExit::from_app(&app, entered.elapsed()))
}

#[cfg(test)]
mod tests;
