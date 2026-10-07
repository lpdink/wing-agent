//! Ask 映射（03 步）：wing 的两类 `ask` 事件 → ACP 的正式交互面。
//!
//! 三径分流（形态判据 = `Ask` 事件的字段，沿用 01 的占位实现）：
//!
//! | 形态 | 出路 |
//! |------|------|
//! | `required` + `choices`（Bash 危险命令确认，retired 形态） | `session/request_permission`：choices 恰为 `{y,n,yolo}` 时三选项友好映射（token 回写）；其它 choices 按 label 生成选项并回 label |
//! | `questions`（AskUserQuestion，1–4 题） | `elicitation/create`（form 模式，**能力门控**） |
//! | 同上但客户端无 elicitation 能力（或回 `-32601`） | 回退：逐题串行 `session/request_permission` |
//!
//! 答案经 WS `ClientRequest{content, tool_call_id}` 定向回写 feedback waiter，内容格式是
//! **逐字契约**（常量直接复用 TUI 面板的 `shared::panels::ask`）：
//!
//! - `questions` 形态：每题一行 `header: answer`（header 空回退 id）；多选 label 以 `", "`
//!   连接；未答 `(user did not answer)`；取消 `__wing_ask_cancelled__`；
//! - Bash 形态：**裸 token**（`y` / `n` / `yolo`）——后端 `_parse_feedback` 只认这三个，
//!   取消哨兵会被判成无效选项并**反复追问**（等价于把轮次挂到 6000s 超时）。
//!
//! 并发语义（design D5）：ask 处理**就地 await** 在轮次事件循环里——同一会话至多一个在途
//! agent→client 请求，后续 ask 留在会话事件缓冲里排队（与 TUI `ask_panels` 的 FIFO 队列
//! 同语义）。因此答案永远在轮次内回写，不需要 detached 任务的簿记，也不存在「终态之后才到
//! 的答案」竞态。等待时同时等 `SessionHub::stream_dead()`：WS 一断就用
//! [`default_answer`] 收口（review r1 N-1）。
//!
//! 可测性：整条流程（`resolve_with`）只依赖 [`AskInteraction`] 这一小片交互面，
//! 单测用脚本化 mock 驱动，不需要真实连接。

use std::future::Future;

use agent_client_protocol::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::CreateElicitationRequest;
use agent_client_protocol::schema::v1::CreateElicitationResponse;
use agent_client_protocol::schema::v1::ElicitationAction;
use agent_client_protocol::schema::v1::ElicitationContentValue;
use agent_client_protocol::schema::v1::ElicitationFormMode;
use agent_client_protocol::schema::v1::ElicitationMode;
use agent_client_protocol::schema::v1::ElicitationPropertySchema;
use agent_client_protocol::schema::v1::ElicitationSchema;
use agent_client_protocol::schema::v1::ElicitationSessionScope;
use agent_client_protocol::schema::v1::EnumOption;
use agent_client_protocol::schema::v1::ErrorCode;
use agent_client_protocol::schema::v1::MultiSelectPropertySchema;
use agent_client_protocol::schema::v1::PermissionOption;
use agent_client_protocol::schema::v1::PermissionOptionKind;
use agent_client_protocol::schema::v1::RequestPermissionOutcome;
use agent_client_protocol::schema::v1::RequestPermissionRequest;
use agent_client_protocol::schema::v1::RequestPermissionResponse;
use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::schema::v1::StringPropertySchema;
use agent_client_protocol::schema::v1::ToolCallId;
use agent_client_protocol::schema::v1::ToolCallUpdate;
use agent_client_protocol::schema::v1::ToolCallUpdateFields;
use agent_client_protocol::schema::v1::ToolKind;

use crate::protocol::AskQuestion;
use crate::protocol::WingEvent;
use crate::shared::panels::ask::ASK_CANCEL_CONTENT;
use crate::shared::panels::ask::UNANSWERED_PLACEHOLDER;

use super::session::SessionHub;

// ============================================================
// 常量
// ============================================================

/// Bash 确认的回写 token 之一：执行（`allow_once`）。
const BASH_TOKEN_Y: &str = "y";

/// Bash 确认的回写 token：执行并打开 yolo 模式（`allow_always`）。
const BASH_TOKEN_YOLO: &str = "yolo";

/// Bash 确认的回写 token：拒绝（`reject_once`）。也是**任何**异常结果的默认值。
const BASH_TOKEN_N: &str = "n";

/// Bash 危险命令确认的 token 集合（`bash.py` 的 `_DANGEROUS_CHOICES`）。
///
/// 只有 `choices` 恰好是这一组时才走友好映射（三选项 + 回 token）：后端
/// `_parse_feedback` 只认这三个，别的词回过去会被判无效并反复追问。
const DANGEROUS_CHOICES: [&str; 3] = [BASH_TOKEN_Y, BASH_TOKEN_N, BASH_TOKEN_YOLO];

/// 回退路径里「跳过本题」选项的 `optionId`（`reject_once`）。
const SKIP_OPTION_ID: &str = "__wing_skip__";

/// 回退路径里自由文本题的「无答案继续」选项的 `optionId`（`allow_once`）。
///
/// 与 [`SKIP_OPTION_ID`] 语义相同（本题未答、继续下一题），但必须单独存在：
/// 权限卡片只能给按钮、收不到自由文本，若一道自由文本题**只有** `reject_*` 一条选项，
/// 只提供「同意 / 拒绝」二态的客户端（omnigent 的 yes/no 桥）在「同意」时会找不到
/// `allow_*` 而回 `cancelled` outcome——那被我们判成「轮次已取消」，后续题目整批不再询问。
/// 补一条正面的 `allow_once` 后：这类客户端会渲染选项卡（≥2 条 + 含 `reject_*`），
/// 「同意」也落到 `selected`（未答 + 继续），不再撞上 `cancelled`。
const CONTINUE_OPTION_ID: &str = "__wing_continue__";

/// 回退路径里题目选项的 `optionId` 前缀（其后是 `{题号}_{选项号}`）。
///
/// 用保留前缀 + 下标生成，而不是直接拿 label 当 `optionId`：后端只保证 label 非空且
/// 同题内唯一，没有保留名约定——某个 label 恰好叫 `__wing_skip__` 时，客户端选它会被
/// 当成「跳过」。
const OPTION_ID_PREFIX: &str = "__wing_opt_";

/// 自由文本题的 `description` 提示（单选/多选题的提示由各选项的 description 承担）。
const FREE_FORM_HINT: &str = "Type your answer";

// ============================================================
// 形态分类
// ============================================================

/// `ask` 事件的形态视图（借自事件字段）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AskView<'a> {
    /// AskUserQuestion：`questions` 形态（1–4 题）。
    Questions(&'a [AskQuestion]),
    /// retired 单问题形态（`questions` 空 + `required` + `choices` 非空）：
    /// Bash 危险命令确认，或未来别的「必须从这些 label 里选一个」的询问。
    RequiredChoice {
        /// 必须被选中的 label 集合（Bash 形态 = `["y","n","yolo"]`）。
        choices: &'a [String],
    },
    /// 未分类形态（当前后端不会产生）：防御性回退（回取消哨兵，见 `resolve_with`）。
    Unclassified,
}

/// 识别一条 `ask` 事件：返回 `(tool_call_id, 形态)`。
///
/// `None` = 不可应答（非 `ask` 事件 / 空 `tool_call_id`），已记日志。
/// 空 id 的判据：后端 `ToolContext::ask_feedback` 恒注入非空 id
/// （`_current_tool_call_id.get() or uuid.uuid4().hex`，`agent/core.py`），
/// 空 id 只可能来自异常来源——回不了 waiter，跳过而不是瞎答。
pub fn classify(event: &WingEvent) -> Option<(&str, AskView<'_>)> {
    let WingEvent::Ask {
        tool_call_id,
        questions,
        choices,
        required,
        ..
    } = event
    else {
        return None;
    };
    if tool_call_id.is_empty() {
        tracing::warn!("acp: ask event without a tool_call_id; dropped (cannot route an answer)");
        return None;
    }
    let view = if !questions.is_empty() {
        AskView::Questions(questions)
    } else if *required && !choices.is_empty() {
        AskView::RequiredChoice { choices }
    } else {
        AskView::Unclassified
    };
    Some((tool_call_id, view))
}

// ============================================================
// 交互面
// ============================================================

/// ask 处理所需的一小片 ACP 客户端交互面。
///
/// trait 的唯一目的是**可测性**：单测注入脚本化应答，不必建真连接/真网关。
pub trait AskInteraction {
    /// `session/request_permission`（agent → client 请求）。
    fn permission(
        &self,
        request: RequestPermissionRequest,
    ) -> impl Future<Output = Result<RequestPermissionResponse, Error>> + Send;

    /// `elicitation/create`（agent → client 请求）。
    fn elicitation(
        &self,
        request: CreateElicitationRequest,
    ) -> impl Future<Output = Result<CreateElicitationResponse, Error>> + Send;
}

/// 真实交互面：请求发给 ACP 客户端。
///
/// `block_task` 要求调用方**已在 dispatch loop 之外**（cookbook 的用法约定）——
/// ask 处理跑在 `cx.spawn` 出去的轮次任务里，满足前提。
struct ConnectionAsk<'a> {
    connection: &'a ConnectionTo<Client>,
}

impl AskInteraction for ConnectionAsk<'_> {
    fn permission(
        &self,
        request: RequestPermissionRequest,
    ) -> impl Future<Output = Result<RequestPermissionResponse, Error>> + Send {
        self.connection.send_request(request).block_task()
    }

    fn elicitation(
        &self,
        request: CreateElicitationRequest,
    ) -> impl Future<Output = Result<CreateElicitationResponse, Error>> + Send {
        self.connection.send_request(request).block_task()
    }
}

// ============================================================
// 入口
// ============================================================

/// 一次 ask 处理的应答结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOutcome {
    /// 回写内容（经 WS `ClientRequest{content, tool_call_id}` 定向 resolve feedback waiter）。
    pub answer: String,
    /// `elicitation/create` 被客户端回 `-32601`：调用方应把进程级 elicitation 能力**降级**
    /// （粘性，此后一律走回退路径）。
    pub elicitation_unsupported: bool,
}

impl AskOutcome {
    fn answer(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
            elicitation_unsupported: false,
        }
    }
}

/// 处理一条 `ask` 事件：分类 → 三径分流 → 返回要回写的答案。
///
/// `None` = 该事件不可应答（非 `ask` / 空 `tool_call_id`，已记 warn）。
pub async fn resolve(
    hub: &SessionHub,
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    event: &WingEvent,
) -> Option<AskOutcome> {
    let (tool_call_id, view) = classify(event)?;
    let interaction = ConnectionAsk { connection };
    Some(
        resolve_with(
            &interaction,
            hub.elicitation_form_supported(),
            session_id,
            tool_call_id,
            view,
        )
        .await,
    )
}

/// [`resolve`] 的内层：与 hub / 真实连接无关的分流流程（单测直接驱动这一层）。
async fn resolve_with(
    interaction: &impl AskInteraction,
    elicitation_ok: bool,
    session_id: &SessionId,
    tool_call_id: &str,
    view: AskView<'_>,
) -> AskOutcome {
    match view {
        AskView::RequiredChoice { choices } => AskOutcome::answer(
            required_choice(interaction, session_id, tool_call_id, choices).await,
        ),
        AskView::Questions(questions) => {
            questions_answer(
                interaction,
                elicitation_ok,
                session_id,
                tool_call_id,
                questions,
            )
            .await
        }
        AskView::Unclassified => {
            // 当前后端不会产生这种形态（只有 bash.py / ask_user.py 两个 ask 生产者）：
            // 记 warn 并按「用户取消」收口，与 01 的占位同口径——绝不悬挂轮次。
            tracing::warn!(
                tool_call_id,
                "acp: ask without questions nor required choices; answering as cancelled"
            );
            AskOutcome::answer(ASK_CANCEL_CONTENT)
        }
    }
}

/// 事件流已死（`hub` 已进入收尾）而询问还没答完时的**默认答案**。
///
/// 与「无人可问」的语义一致：Bash / required 形态 = 拒绝（已知 token 形态）或取消哨兵
/// （泛化形态）；`questions` = 逐题未答占位；未分类 = 取消哨兵。轮次随后会走到既有的
/// 「gateway event stream ended」错误分支，答案本身多半已递不出去（WS 已断）——它的作用是
/// 让这条路径与正常收口共用同一段代码（绝不留一个悬挂的 waiter）。
///
/// `None` = 这条事件本来就不该应答（非 `ask` / 空 `tool_call_id`）。
pub fn default_answer(event: &WingEvent) -> Option<AskOutcome> {
    let (_, view) = classify(event)?;
    let answer = match view {
        AskView::RequiredChoice { choices } => required_choice_default(choices).to_string(),
        AskView::Questions(questions) => questions
            .iter()
            .map(|question| format!("{}: {}", question.tab_label(), UNANSWERED_PLACEHOLDER))
            .collect::<Vec<_>>()
            .join("\n"),
        AskView::Unclassified => ASK_CANCEL_CONTENT.to_string(),
    };
    Some(AskOutcome::answer(answer))
}

// ============================================================
// ① required 形态（Bash 危险命令确认 / 未来别的必选询问）→ permission
// ============================================================

/// `choices` 是否恰好是 Bash 的三个 token（顺序无关）。
fn is_dangerous_choices(choices: &[String]) -> bool {
    choices.len() == DANGEROUS_CHOICES.len()
        && DANGEROUS_CHOICES
            .iter()
            .all(|token| choices.iter().any(|choice| choice == token))
}

/// required 形态：`session/request_permission`。
///
/// - 已知 Bash 三 token：三选项的 `optionId` 就是回写 token（友好名字 + 对应 kind）；
/// - 其它 `required+choices`（当前无生产者，防御性泛化）：每个 label 一条 `allow_once`，
///   回选中的 label（与 TUI 的 `RequiredChoice` 同口径：回裸 label）。
async fn required_choice(
    interaction: &impl AskInteraction,
    session_id: &SessionId,
    tool_call_id: &str,
    choices: &[String],
) -> String {
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(
            // 卡片已由 `tool_call` 事件建立（title = 命令首行、rawInput = 参数）：
            // 这里只补 kind（客户端的字段合并语义，不会覆盖标题；凭空造 title 反而会把
            // 命令行换成警告文案）。
            ToolCallId::new(tool_call_id.to_string()),
            ToolCallUpdateFields::new().kind(ToolKind::Execute),
        ),
        required_choice_options(choices),
    );
    match interaction.permission(request).await {
        Ok(response) => required_choice_answer(choices, &response.outcome),
        Err(err) => {
            tracing::warn!(
                tool_call_id,
                error = %err,
                "acp: required-choice permission request failed; answering with the default"
            );
            required_choice_default(choices).to_string()
        }
    }
}

/// 请求失败 / 事件流已死时的默认答案：已知 Bash 形态 = 拒绝（`n`）；泛化形态 = 取消哨兵
/// （不伪造某个 label，见 [`required_choice_answer`]）。
fn required_choice_default(choices: &[String]) -> &'static str {
    if is_dangerous_choices(choices) {
        BASH_TOKEN_N
    } else {
        ASK_CANCEL_CONTENT
    }
}

/// 选项集合：已知 Bash 形态用友好三选项（token 即 `optionId`）；
/// 泛化形态每个 label 一条 `allow_once`（`optionId` = label，客户端原样回传）。
fn required_choice_options(choices: &[String]) -> Vec<PermissionOption> {
    if is_dangerous_choices(choices) {
        return bash_options();
    }
    choices
        .iter()
        .map(|label| {
            PermissionOption::new(
                label.clone(),
                label.clone(),
                PermissionOptionKind::AllowOnce,
            )
        })
        .collect()
}

/// 三个友好选项（`optionId` 即回写 token，客户端原样回传，映射恒等）。
fn bash_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(BASH_TOKEN_Y, "Yes, run it", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            BASH_TOKEN_YOLO,
            "Yes, and don't ask again",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(BASH_TOKEN_N, "No", PermissionOptionKind::RejectOnce),
    ]
}

/// permission 结果 → 回写内容。
///
/// - 已知 Bash 形态：只认 `y`/`yolo`/`n`；`cancelled` outcome、未知 `optionId`、请求出错
///   一律回 `n`（拒绝）——后端只认这三个 token，**绝不能**用取消哨兵
///   （`_parse_feedback` 会判无效并重新追问）。
/// - 泛化形态：回选中的 label（与 TUI `RequiredChoice` 同口径）；`cancelled` / 未知 id →
///   取消哨兵——这类形态的语义未知，伪造一个 label 比明确说「取消了」更危险。
fn required_choice_answer(choices: &[String], outcome: &RequestPermissionOutcome) -> String {
    let RequestPermissionOutcome::Selected(selected) = outcome else {
        return required_choice_default(choices).to_string();
    };
    let option_id = selected.option_id.0.as_ref();
    if !is_dangerous_choices(choices) {
        if choices.iter().any(|label| label == option_id) {
            return option_id.to_string();
        }
        tracing::warn!(
            option_id,
            "acp: unknown option for a required-choice ask; answering as cancelled"
        );
        return ASK_CANCEL_CONTENT.to_string();
    }
    match option_id {
        BASH_TOKEN_Y => BASH_TOKEN_Y.to_string(),
        BASH_TOKEN_YOLO => BASH_TOKEN_YOLO.to_string(),
        BASH_TOKEN_N => BASH_TOKEN_N.to_string(),
        option_id => {
            tracing::debug!(
                option_id,
                "acp: unexpected permission option for a bash confirmation; rejecting"
            );
            BASH_TOKEN_N.to_string()
        }
    }
}

// ============================================================
// ② AskUserQuestion → elicitation
// ============================================================

/// `questions` 形态：有 elicitation 能力走表单，否则/失败走回退。
async fn questions_answer(
    interaction: &impl AskInteraction,
    elicitation_ok: bool,
    session_id: &SessionId,
    tool_call_id: &str,
    questions: &[AskQuestion],
) -> AskOutcome {
    if elicitation_ok {
        let request = elicitation_request(session_id, tool_call_id, questions);
        match interaction.elicitation(request).await {
            Ok(response) => {
                return AskOutcome::answer(elicitation_answer(questions, &response));
            }
            Err(err) => {
                // `-32601` = 客户端没这个方法：进程内永久降级（粘性），本步起不再试探。
                // 其余错误（internal / cancelled / 连接关闭）只回退、不降级——一次瞬时
                // 故障不该把表单能力永久关掉。
                let downgrade = err.code == ErrorCode::MethodNotFound;
                tracing::warn!(
                    tool_call_id,
                    code = %err.code,
                    message = %err.message,
                    downgrade,
                    "acp: elicitation/create failed; falling back to per-question permissions"
                );
                return AskOutcome {
                    answer: fallback_questions(interaction, session_id, tool_call_id, questions)
                        .await,
                    elicitation_unsupported: downgrade,
                };
            }
        }
    }
    AskOutcome::answer(fallback_questions(interaction, session_id, tool_call_id, questions).await)
}

/// 表单请求：`scope` 带 `toolCallId`（客户端据此把表单挂到工具卡片上）。
fn elicitation_request(
    session_id: &SessionId,
    tool_call_id: &str,
    questions: &[AskQuestion],
) -> CreateElicitationRequest {
    let scope = ElicitationSessionScope::new(session_id.clone())
        .tool_call_id(ToolCallId::new(tool_call_id.to_string()));
    CreateElicitationRequest::new(
        ElicitationMode::Form(ElicitationFormMode::new(
            scope,
            elicitation_schema(questions),
        )),
        elicitation_message(questions),
    )
}

/// 表单 `message`：单题取问题全文（空则回退 `header → id`），多题取 `"N questions"`
/// （逐题标题在 schema 里，不重复塞进 message）。
fn elicitation_message(questions: &[AskQuestion]) -> String {
    match questions {
        [only] => question_title(only),
        questions => format!("{} questions", questions.len()),
    }
}

/// 表单 schema：每题一个 property（key = `question.id`，标题 = 问题全文），全部 `required=false`
/// ——wing 允许未答（TUI 的 Submit 不设门，未答出占位行）。
fn elicitation_schema(questions: &[AskQuestion]) -> ElicitationSchema {
    let mut schema = ElicitationSchema::new();
    for question in questions {
        schema = schema.property(question.id.clone(), question_property(question), false);
    }
    schema
}

/// 单题的 property：多选 → `array`（titled items）；单选 → `string` + `oneOf`；
/// 无选项 → 自由文本 `string`。
fn question_property(question: &AskQuestion) -> ElicitationPropertySchema {
    let title = question_title(question);
    let options = enum_options(question);
    if options.is_empty() {
        return ElicitationPropertySchema::String(
            StringPropertySchema::new()
                .title(title)
                .description(FREE_FORM_HINT),
        );
    }
    if question.multi_select {
        ElicitationPropertySchema::Array(MultiSelectPropertySchema::titled(options).title(title))
    } else {
        ElicitationPropertySchema::String(StringPropertySchema::new().title(title).one_of(options))
    }
}

/// 题目标题：问题全文；空则回退 `header → id`（与 TUI 的 tab label 同口径）。
fn question_title(question: &AskQuestion) -> String {
    let text = question.question.trim();
    if text.is_empty() {
        question.tab_label().to_string()
    } else {
        text.to_string()
    }
}

/// 题目选项的归一化视图（`options` 为主；只有旧记录才填纯字符串 `choices`，
/// 与 TUI `AskPanel` 的 `normalize_question` 同口径）。
fn enum_options(question: &AskQuestion) -> Vec<EnumOption> {
    let normalized: Vec<(&str, &str)> = if question.options.is_empty() {
        question
            .choices
            .iter()
            .map(|label| (label.as_str(), ""))
            .collect()
    } else {
        question
            .options
            .iter()
            .map(|option| (option.label.as_str(), option.description.as_str()))
            .collect()
    };
    normalized
        .into_iter()
        .map(|(label, description)| {
            let option = EnumOption::new(label, label);
            let description = description.trim();
            if description.is_empty() {
                option
            } else {
                option.description(description)
            }
        })
        .collect()
}

/// elicitation 应答 → 回写内容。
fn elicitation_answer(questions: &[AskQuestion], response: &CreateElicitationResponse) -> String {
    match &response.action {
        ElicitationAction::Accept(accept) => answer_lines(questions, accept.content.as_ref()),
        ElicitationAction::Decline | ElicitationAction::Cancel => ASK_CANCEL_CONTENT.to_string(),
        action => {
            // `ElicitationAction` 是 non_exhaustive：未知 action 按「用户取消」收口。
            tracing::warn!(
                action = ?action,
                "acp: unknown elicitation action; answering as cancelled"
            );
            ASK_CANCEL_CONTENT.to_string()
        }
    }
}

/// 逐题一行 `header: answer`。
///
/// **逐字契约**：格式与 TUI 的 `AskPanel::build_response` 一致，未答占位直接复用它的
/// 常量（`shared::panels::ask`）——一致由同一份常量保证，不靠两处字符串巧合相等。
fn answer_lines(
    questions: &[AskQuestion],
    content: Option<&std::collections::BTreeMap<String, ElicitationContentValue>>,
) -> String {
    questions
        .iter()
        .map(|question| {
            let value = content.and_then(|content| content.get(&question.id));
            format!("{}: {}", question.tab_label(), answer_text(value))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 单题答案文本：`StringArray`（多选）过滤空项后以 `", "` 连接（与 TUI 的多选同格式）；
/// `String` 原样；缺失 / 空 / 空白 / 类型不符 → 未答占位。
fn answer_text(value: Option<&ElicitationContentValue>) -> String {
    let text = match value {
        Some(ElicitationContentValue::StringArray(labels)) => labels
            .iter()
            .map(|label| label.trim())
            .filter(|label| !label.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Some(ElicitationContentValue::String(text)) => text.trim().to_string(),
        // number / integer / boolean 与未来的变体：wing 的题型只有选择与自由文本，
        // 对不上的值一律按未答（不猜）。
        _ => String::new(),
    };
    if text.is_empty() {
        UNANSWERED_PLACEHOLDER.to_string()
    } else {
        text
    }
}

// ============================================================
// ③ 回退：逐题串行 permission
// ============================================================

/// 回退路径：逐题串行 `session/request_permission`，返回逐题 `header: answer` 行。
///
/// 串行（而不是并发发 N 张卡）是刻意的：一次只挂一张卡片，题与题之间无共享状态，
/// 答案按题目顺序逐条累积；客户端对同题并发授权的语义各异，不值得赌。
///
/// **`cancelled` outcome = 轮次已被取消**（ACP 规范对该 outcome 的定义），此时不再追问
/// 后续题目（后面的卡片发给一个正在取消的轮次只会让用户对着空气按按钮），剩余题目按
/// 未答占位收口并立刻回写——prompt 不必等用户把多余的卡片一张张关掉。
///
/// 注意：**只有 `cancelled` 才停**。看板客户端在「同意」时未必回 `selected`——只提供
/// Approve/Reject 二态的实现（omnigent 的 yes/no 桥）在找不到 `allow_*` 选项时会回
/// `cancelled`（`_permission_outcome` 的兜底）。因此每道题**必须**至少有一条 `allow_once`
/// 选项（见 [`fallback_options`]），否则它后面的题目会被整批吞掉（review r1 S-1）。
async fn fallback_questions(
    interaction: &impl AskInteraction,
    session_id: &SessionId,
    tool_call_id: &str,
    questions: &[AskQuestion],
) -> String {
    let mut answers: Vec<String> = Vec::with_capacity(questions.len());
    for (index, question) in questions.iter().enumerate() {
        let Some(answer) =
            fallback_question(interaction, session_id, tool_call_id, index, question).await
        else {
            break;
        };
        answers.push(answer);
    }
    // 剩下的（轮次取消 / 已答完）一律未答占位：无论客户端怎么选，轮次都不悬挂。
    while answers.len() < questions.len() {
        answers.push(UNANSWERED_PLACEHOLDER.to_string());
    }
    questions
        .iter()
        .zip(answers)
        .map(|(question, answer)| format!("{}: {}", question.tab_label(), answer))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 单题一张权限卡片；`None` = 客户端取消了轮次（不追问后续题目）。
/// 请求失败按「未答」收口但**继续**问下一题（单题故障不牵连整轮）。
async fn fallback_question(
    interaction: &impl AskInteraction,
    session_id: &SessionId,
    tool_call_id: &str,
    question_index: usize,
    question: &AskQuestion,
) -> Option<String> {
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(
            // 合成的卡片 id：同一真实 toolCallId 上的第二次授权会让客户端把上一次判为取消
            // （Zed 的 upsert 语义），逐题 id 才能让每题独立成立。
            ToolCallId::new(format!("{tool_call_id}:{}", question.id)),
            ToolCallUpdateFields::new()
                .title(question_title(question))
                .kind(ToolKind::Other),
        ),
        fallback_options(question, question_index),
    );
    match interaction.permission(request).await {
        Ok(response) => fallback_choice_answer(question, question_index, &response.outcome),
        Err(err) => {
            tracing::warn!(
                tool_call_id,
                question_id = %question.id,
                error = %err,
                "acp: fallback permission request failed; question left unanswered"
            );
            Some(UNANSWERED_PLACEHOLDER.to_string())
        }
    }
}

/// 题目选项的保留 `optionId`：`__wing_opt_{题号}_{选项号}`。
///
/// 不用 label 当 `optionId`：后端只保证 label 非空且题内唯一，没有保留名约定——某个
/// label 恰好叫 `__wing_skip__` 时，客户端选它会被当成「跳过」。
fn option_id_for(question_index: usize, option_index: usize) -> String {
    format!("{OPTION_ID_PREFIX}{question_index}_{option_index}")
}

/// 该题的选项（`allow_once`，`optionId` 见 [`option_id_for`]）+ 两条逃生选项。
///
/// 逃生选项（本题未答、继续下一题）：
///
/// - `Skip`（`reject_once`，[`SKIP_OPTION_ID`]）——「拒绝 / 跳过」语义的落点；
/// - 自由文本题**额外**一条 `Continue`（`allow_once`，[`CONTINUE_OPTION_ID`]）——没有它，
///   只给 Approve/Reject 的客户端（权限卡片收不到自由文本）在「同意」时会回 `cancelled`
///   outcome，被我们按规范判成「轮次已取消」，后面的题目整批不再询问（review r1 S-1）。
///   顺带让选项数 ≥2 且含 `reject_*`，这类客户端会直接渲染选项卡而不是二态卡。
fn fallback_options(question: &AskQuestion, question_index: usize) -> Vec<PermissionOption> {
    let options = enum_options(question);
    let mut fallback: Vec<PermissionOption> = options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            PermissionOption::new(
                option_id_for(question_index, index),
                option.value.clone(),
                PermissionOptionKind::AllowOnce,
            )
        })
        .collect();
    if options.is_empty() {
        fallback.push(PermissionOption::new(
            CONTINUE_OPTION_ID,
            "Continue",
            PermissionOptionKind::AllowOnce,
        ));
    }
    fallback.push(PermissionOption::new(
        SKIP_OPTION_ID,
        "Skip",
        PermissionOptionKind::RejectOnce,
    ));
    fallback
}

/// 选项 → 答案：题目选项按保留 id 映射回 label；`Continue` / `Skip` = 未答（继续下一题）；
/// 未知 id = 未答 + warn；`None` = `cancelled` outcome（轮次已取消，见 [`fallback_questions`]）。
fn fallback_choice_answer(
    question: &AskQuestion,
    question_index: usize,
    outcome: &RequestPermissionOutcome,
) -> Option<String> {
    let RequestPermissionOutcome::Selected(selected) = outcome else {
        return None;
    };
    let chosen = selected.option_id.0.as_ref();
    if chosen == SKIP_OPTION_ID || chosen == CONTINUE_OPTION_ID {
        return Some(UNANSWERED_PLACEHOLDER.to_string());
    }
    Some(
        enum_options(question)
            .into_iter()
            .enumerate()
            .find(|(index, _)| option_id_for(question_index, *index) == chosen)
            .map(|(_, option)| option.value)
            .unwrap_or_else(|| {
                tracing::warn!(
                    question_id = %question.id,
                    option_id = chosen,
                    "acp: fallback picked an option we did not offer; question left unanswered"
                );
                UNANSWERED_PLACEHOLDER.to_string()
            }),
    )
}

// ============================================================
// 单测
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use agent_client_protocol::schema::v1::ElicitationAcceptAction;
    use agent_client_protocol::schema::v1::ElicitationScope;
    use agent_client_protocol::schema::v1::MultiSelectItems;
    use agent_client_protocol::schema::v1::SelectedPermissionOutcome;
    use serde_json::json;
    use tokio::sync::oneshot;

    // ---- fixtures ----

    fn session() -> SessionId {
        SessionId::new("s1")
    }

    fn event(value: serde_json::Value) -> WingEvent {
        let rendered = value.clone();
        serde_json::from_value(value)
            .unwrap_or_else(|err| panic!("fixture must decode: {err}\n{rendered}"))
    }

    /// Bash 危险命令确认（bash.py 的真实形状：`required` + `y/n/yolo`，无 questions）。
    fn bash_ask() -> WingEvent {
        event(json!({
            "type": "ask",
            "tool_call_id": "tc_bash",
            "question": "⚠️ Dangerous command detected:\n```bash\nrm -rf x\n```\nProceed?",
            "choices": ["y", "n", "yolo"],
            "required": true,
            "created_at": "c",
            "session_id": "s1",
            "request_id": "r",
        }))
    }

    /// AskUserQuestion：单选带选项 / 多选带选项 / 自由文本（三种题型一次覆盖）。
    fn questions_ask() -> WingEvent {
        event(json!({
            "type": "ask",
            "tool_call_id": "tc_ask",
            "questions": [
                {"id": "theme", "header": "配色", "question": "选一个配色",
                 "options": [{"label": "浅色"}, {"label": "深色", "description": "夜间用"}]},
                {"id": "features", "header": "", "question": "要哪些功能", "multiSelect": true,
                 "options": [{"label": "多选"}, {"label": "预览"}]},
                {"id": "name", "header": "名字", "question": "叫什么名字"}
            ],
            "created_at": "c",
            "session_id": "s1",
            "request_id": "r",
        }))
    }

    fn questions_of(event: &WingEvent) -> &[AskQuestion] {
        match event {
            WingEvent::Ask { questions, .. } => questions,
            other => panic!("expected an ask event, got {}", other.event_type()),
        }
    }

    /// 一条非 ask 事件（分类与默认答案都应直接拒绝它）。
    fn text_event() -> WingEvent {
        event(json!({
            "type": "text", "content": "hi", "created_at": "c", "session_id": "s1", "request_id": "r",
        }))
    }

    fn selected(option_id: &str) -> RequestPermissionResponse {
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(option_id.to_string()),
        ))
    }

    fn permission_cancelled() -> RequestPermissionResponse {
        RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
    }

    fn accepted(entries: &[(&str, serde_json::Value)]) -> CreateElicitationResponse {
        let content: BTreeMap<String, ElicitationContentValue> = entries
            .iter()
            .map(|(key, value)| {
                (
                    (*key).to_string(),
                    serde_json::from_value(value.clone()).expect("content value decodes"),
                )
            })
            .collect();
        CreateElicitationResponse::new(ElicitationAction::Accept(
            ElicitationAcceptAction::new().content(content),
        ))
    }

    fn declined(action: ElicitationAction) -> CreateElicitationResponse {
        CreateElicitationResponse::new(action)
    }

    // ---- 脚本化交互面 ----

    /// 预置的 permission 应答：立即 / 挂起到测试侧放行（后者用来断言串行性）。
    enum ScriptedPermission {
        Ready(Result<RequestPermissionResponse, Error>),
        Deferred(oneshot::Receiver<Result<RequestPermissionResponse, Error>>),
    }

    /// 脚本化 [`AskInteraction`]：按顺序吐出预置应答，并记录请求（顺序 + 全量请求体）。
    #[derive(Default)]
    struct ScriptedAsk {
        permissions: Mutex<VecDeque<ScriptedPermission>>,
        elicitations: Mutex<VecDeque<Result<CreateElicitationResponse, Error>>>,
        permission_log: Mutex<Vec<String>>,
        elicitation_log: Mutex<Vec<String>>,
        seen_permissions: Mutex<Vec<RequestPermissionRequest>>,
        seen_elicitations: Mutex<Vec<CreateElicitationRequest>>,
    }

    impl ScriptedAsk {
        fn new() -> Self {
            Self::default()
        }

        fn ready_permissions(
            replies: impl IntoIterator<Item = Result<RequestPermissionResponse, Error>>,
        ) -> Self {
            let mock = Self::new();
            mock.push_permissions(replies);
            mock
        }

        fn push_permissions(
            &self,
            replies: impl IntoIterator<Item = Result<RequestPermissionResponse, Error>>,
        ) {
            self.permissions
                .lock()
                .expect("mock lock")
                .extend(replies.into_iter().map(ScriptedPermission::Ready));
        }

        fn push_deferred(
            &self,
            receiver: oneshot::Receiver<Result<RequestPermissionResponse, Error>>,
        ) {
            self.permissions
                .lock()
                .expect("mock lock")
                .push_back(ScriptedPermission::Deferred(receiver));
        }

        fn push_elicitation(&self, reply: Result<CreateElicitationResponse, Error>) {
            self.elicitations
                .lock()
                .expect("mock lock")
                .push_back(reply);
        }

        fn permission_log(&self) -> Vec<String> {
            self.permission_log.lock().expect("mock lock").clone()
        }

        fn elicitation_log(&self) -> Vec<String> {
            self.elicitation_log.lock().expect("mock lock").clone()
        }

        fn seen_permissions(&self) -> Vec<RequestPermissionRequest> {
            self.seen_permissions.lock().expect("mock lock").clone()
        }

        fn seen_elicitations(&self) -> Vec<CreateElicitationRequest> {
            self.seen_elicitations.lock().expect("mock lock").clone()
        }
    }

    impl AskInteraction for ScriptedAsk {
        fn permission(
            &self,
            request: RequestPermissionRequest,
        ) -> impl Future<Output = Result<RequestPermissionResponse, Error>> + Send {
            let id = request.tool_call.tool_call_id.to_string();
            self.permission_log
                .lock()
                .expect("mock lock")
                .push(format!("permission:{id}"));
            self.seen_permissions
                .lock()
                .expect("mock lock")
                .push(request);
            let reply = self
                .permissions
                .lock()
                .expect("mock lock")
                .pop_front()
                .expect("no scripted permission reply left");
            async move {
                match reply {
                    ScriptedPermission::Ready(response) => response,
                    ScriptedPermission::Deferred(receiver) => receiver.await.unwrap_or_else(|_| {
                        Err(Error::internal_error().data("scripted reply dropped"))
                    }),
                }
            }
        }

        fn elicitation(
            &self,
            request: CreateElicitationRequest,
        ) -> impl Future<Output = Result<CreateElicitationResponse, Error>> + Send {
            let id = match &request.mode {
                ElicitationMode::Form(form) => match &form.scope {
                    ElicitationScope::Session(scope) => scope
                        .tool_call_id
                        .as_ref()
                        .map(|id| id.to_string())
                        .unwrap_or_default(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            self.elicitation_log
                .lock()
                .expect("mock lock")
                .push(format!("elicitation:{id}"));
            self.seen_elicitations
                .lock()
                .expect("mock lock")
                .push(request);
            let reply = self
                .elicitations
                .lock()
                .expect("mock lock")
                .pop_front()
                .expect("no scripted elicitation reply left");
            async move { reply }
        }
    }

    // ---- 分类 ----

    #[test]
    fn classify_covers_both_shapes_and_defensive_defaults() {
        let bash = bash_ask();
        let (id, view) = classify(&bash).expect("bash ask classifies");
        assert_eq!(id, "tc_bash");
        assert!(matches!(
            view,
            AskView::RequiredChoice { choices } if choices == ["y", "n", "yolo"]
        ));

        let questions = questions_ask();
        let (id, view) = classify(&questions).expect("questions ask classifies");
        assert_eq!(id, "tc_ask");
        assert!(matches!(view, AskView::Questions(q) if q.len() == 3));

        // 未分类形态（questions 空、非 required+choices）——当前后端不可达的防御分支。
        let unclassified = event(json!({
            "type": "ask", "tool_call_id": "tc_x", "question": "?",
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }));
        assert_eq!(
            classify(&unclassified).map(|(_, view)| view),
            Some(AskView::Unclassified)
        );

        // 空 tool_call_id：回不了 waiter，跳过。
        let empty_id = event(json!({
            "type": "ask", "tool_call_id": "", "required": true, "choices": ["y", "n", "yolo"],
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }));
        assert!(classify(&empty_id).is_none());

        // 非 ask 事件。
        let text = event(json!({
            "type": "text", "content": "hi", "created_at": "c", "session_id": "s1", "request_id": "r",
        }));
        assert!(classify(&text).is_none());
    }

    #[tokio::test]
    async fn unclassified_ask_cancels_without_any_request() {
        let mock = ScriptedAsk::new();
        let outcome = resolve_with(&mock, true, &session(), "tc_x", AskView::Unclassified).await;
        assert_eq!(outcome.answer, ASK_CANCEL_CONTENT);
        assert!(!outcome.elicitation_unsupported);
        assert!(mock.permission_log().is_empty(), "不得发起任何请求");
        assert!(mock.elicitation_log().is_empty(), "不得发起任何请求");
    }

    /// N-1：事件流已死时的默认答案（并发问都不发，纯函数）。
    #[test]
    fn default_answer_covers_every_shape() {
        assert_eq!(
            default_answer(&bash_ask()).map(|outcome| outcome.answer),
            Some("n".to_string()),
            "Bash 形态：拒绝（后端只认 y/n/yolo）"
        );
        assert_eq!(
            default_answer(&questions_ask()).map(|outcome| outcome.answer),
            Some(
                "配色: (user did not answer)\nfeatures: (user did not answer)\n名字: (user did not answer)"
                    .to_string()
            ),
            "questions 形态：逐题未答占位"
        );
        let unclassified = event(json!({
            "type": "ask", "tool_call_id": "tc_x", "question": "?",
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }));
        assert_eq!(
            default_answer(&unclassified).map(|outcome| outcome.answer),
            Some(ASK_CANCEL_CONTENT.to_string())
        );

        // 不可应答的事件 → None；泛化 required 形态 → 取消哨兵。
        assert!(default_answer(&text_event()).is_none());
        let empty_id = event(json!({
            "type": "ask", "tool_call_id": "", "required": true, "choices": ["y", "n", "yolo"],
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }));
        assert!(default_answer(&empty_id).is_none());
        let generic = event(json!({
            "type": "ask", "tool_call_id": "tc_g", "required": true, "choices": ["yes", "no"],
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }));
        assert_eq!(
            default_answer(&generic).map(|outcome| outcome.answer),
            Some(ASK_CANCEL_CONTENT.to_string())
        );
    }

    // ---- ① required 形态（Bash 三 token + 泛化 choices） ----

    /// Bash 危险命令确认的 `choices`（`bash.py::_DANGEROUS_CHOICES`）。
    fn dangerous_choices() -> Vec<String> {
        ["y", "n", "yolo"].iter().map(|t| t.to_string()).collect()
    }

    #[tokio::test]
    async fn bash_confirmation_maps_three_tokens() {
        let choices = dangerous_choices();
        for (reply, expected) in [
            (selected("y"), "y"),
            (selected("yolo"), "yolo"),
            (selected("n"), "n"),
            (selected("surprise"), "n"),
            (permission_cancelled(), "n"),
        ] {
            let mock = ScriptedAsk::ready_permissions([Ok(reply)]);
            let outcome = resolve_with(
                &mock,
                true,
                &session(),
                "tc_bash",
                AskView::RequiredChoice { choices: &choices },
            )
            .await;
            assert_eq!(outcome.answer, expected);
            assert!(!outcome.elicitation_unsupported);
        }
    }

    #[tokio::test]
    async fn bash_confirmation_request_shape() {
        let choices = dangerous_choices();
        let mock = ScriptedAsk::ready_permissions([Ok(selected("y"))]);
        resolve_with(
            &mock,
            true,
            &session(),
            "tc_bash",
            AskView::RequiredChoice { choices: &choices },
        )
        .await;

        let requests = mock.seen_permissions();
        assert_eq!(requests.len(), 1, "只发一条 permission");
        let request = &requests[0];
        assert_eq!(request.session_id.to_string(), "s1");
        assert_eq!(request.tool_call.tool_call_id.to_string(), "tc_bash");
        assert_eq!(request.tool_call.fields.kind, Some(ToolKind::Execute));
        assert!(
            request.tool_call.fields.title.is_none(),
            "卡片已存在，不覆盖标题（Zed 的字段合并语义）"
        );
        let options: Vec<(String, String, PermissionOptionKind)> = request
            .options
            .iter()
            .map(|option| {
                (
                    option.option_id.to_string(),
                    option.name.clone(),
                    option.kind,
                )
            })
            .collect();
        assert_eq!(
            options,
            vec![
                (
                    "y".into(),
                    "Yes, run it".into(),
                    PermissionOptionKind::AllowOnce
                ),
                (
                    "yolo".into(),
                    "Yes, and don't ask again".into(),
                    PermissionOptionKind::AllowAlways
                ),
                ("n".into(), "No".into(), PermissionOptionKind::RejectOnce),
            ]
        );
    }

    #[tokio::test]
    async fn bash_confirmation_failure_rejects() {
        let choices = dangerous_choices();
        let mock = ScriptedAsk::ready_permissions([Err(Error::internal_error().data("boom"))]);
        let outcome = resolve_with(
            &mock,
            true,
            &session(),
            "tc_bash",
            AskView::RequiredChoice { choices: &choices },
        )
        .await;
        assert_eq!(outcome.answer, "n");
        assert!(!outcome.elicitation_unsupported);
    }

    /// N-2：`choices` 不是 Bash 三 token 时按 label 生成选项、回选中的 label（TUI 同口径）。
    #[tokio::test]
    async fn generic_required_choices_echo_the_selected_label() {
        let choices: Vec<String> = ["yes", "no"].iter().map(|t| t.to_string()).collect();
        let view = || AskView::RequiredChoice { choices: &choices };

        // 选项 = 每条 label 一条 allow_once（optionId = label）。
        let mock = ScriptedAsk::ready_permissions([Ok(selected("no"))]);
        let outcome = resolve_with(&mock, true, &session(), "tc_gen", view()).await;
        assert_eq!(outcome.answer, "no", "回选中的 label，不伪造 y/n/yolo");
        assert_eq!(
            mock.seen_permissions()[0]
                .options
                .iter()
                .map(|option| (option.option_id.to_string(), option.kind))
                .collect::<Vec<_>>(),
            vec![
                ("yes".to_string(), PermissionOptionKind::AllowOnce),
                ("no".to_string(), PermissionOptionKind::AllowOnce),
            ]
        );
        assert_eq!(
            mock.seen_permissions()[0].tool_call.fields.kind,
            Some(ToolKind::Execute)
        );

        // 取消 / 未知 id / 出错 → 取消哨兵（不拿某个 label 冒名顶替）。
        for reply in [permission_cancelled(), selected("y"), selected("yolo")] {
            let mock = ScriptedAsk::ready_permissions([Ok(reply)]);
            let outcome = resolve_with(&mock, true, &session(), "tc_gen", view()).await;
            assert_eq!(outcome.answer, ASK_CANCEL_CONTENT);
        }
        let mock = ScriptedAsk::ready_permissions([Err(Error::internal_error().data("boom"))]);
        let outcome = resolve_with(&mock, true, &session(), "tc_gen", view()).await;
        assert_eq!(outcome.answer, ASK_CANCEL_CONTENT);

        // 顺序无关的已知集合判定：打乱顺序仍是 Bash 形态。
        let shuffled: Vec<String> = ["n", "yolo", "y"].iter().map(|t| t.to_string()).collect();
        assert!(is_dangerous_choices(&shuffled));
        assert!(!is_dangerous_choices(&choices));
        assert!(!is_dangerous_choices(
            &["y", "n"].iter().map(|t| t.to_string()).collect::<Vec<_>>()
        ));
    }

    // ---- ② 表单 ----

    #[test]
    fn elicitation_schema_covers_the_three_question_shapes() {
        let event = questions_ask();
        let schema = elicitation_schema(questions_of(&event));

        assert_eq!(schema.properties.len(), 3);
        assert!(
            schema.required.is_none(),
            "wing 允许未答：所有 property 都 required=false"
        );

        // 单选 + 选项 → string + oneOf（titled enum）。
        match &schema.properties["theme"] {
            ElicitationPropertySchema::String(string) => {
                assert_eq!(string.title.as_deref(), Some("选一个配色"));
                let one_of = string.one_of.as_ref().expect("oneOf");
                assert_eq!(one_of.len(), 2);
                assert_eq!(one_of[0].value, "浅色");
                assert_eq!(one_of[0].title, "浅色");
                assert_eq!(one_of[0].description, None, "无 description 不带字段");
                assert_eq!(one_of[1].description.as_deref(), Some("夜间用"));
            }
            other => panic!("expected a string property, got {other:?}"),
        }

        // 多选 + 选项 → array + titled items。
        match &schema.properties["features"] {
            ElicitationPropertySchema::Array(array) => {
                assert_eq!(array.title.as_deref(), Some("要哪些功能"));
                match &array.items {
                    MultiSelectItems::Titled(items) => {
                        assert_eq!(items.options.len(), 2);
                        assert_eq!(items.options[0].value, "多选");
                        assert_eq!(items.options[1].title, "预览");
                    }
                    other => panic!("expected titled multi-select items, got {other:?}"),
                }
            }
            other => panic!("expected an array property, got {other:?}"),
        }

        // 无选项 → 自由文本 string（带提示）。
        match &schema.properties["name"] {
            ElicitationPropertySchema::String(string) => {
                assert_eq!(string.title.as_deref(), Some("叫什么名字"));
                assert_eq!(string.description.as_deref(), Some(FREE_FORM_HINT));
                assert!(string.one_of.is_none());
            }
            other => panic!("expected a string property, got {other:?}"),
        }
    }

    #[test]
    fn elicitation_message_is_the_question_or_a_count() {
        let single = event(json!({
            "type": "ask", "tool_call_id": "tc", "questions": [
                {"id": "q", "header": "", "question": "只问一句"}
            ],
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }));
        assert_eq!(elicitation_message(questions_of(&single)), "只问一句");

        let many = questions_ask();
        assert_eq!(elicitation_message(questions_of(&many)), "3 questions");
    }

    #[test]
    fn legacy_choices_become_options() {
        let event = event(json!({
            "type": "ask", "tool_call_id": "tc", "questions": [
                {"id": "q", "header": "h", "question": "?", "choices": ["a", "b"]}
            ],
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }));
        let schema = elicitation_schema(questions_of(&event));
        match &schema.properties["q"] {
            ElicitationPropertySchema::String(string) => {
                let one_of = string.one_of.as_ref().expect("choices 折算成 oneOf");
                assert_eq!(one_of.len(), 2);
                assert_eq!(one_of[0].value, "a");
            }
            other => panic!("expected a string property, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn elicitation_request_shape_and_accept_answer() {
        let event = questions_ask();
        let questions = questions_of(&event);
        let mock = ScriptedAsk::new();
        mock.push_elicitation(Ok(accepted(&[
            ("theme", json!("深色")),
            ("features", json!(["多选", "预览"])),
            ("name", json!("wing")),
        ])));

        let outcome = resolve_with(
            &mock,
            true,
            &session(),
            "tc_ask",
            AskView::Questions(questions),
        )
        .await;
        assert_eq!(
            outcome.answer,
            "配色: 深色\nfeatures: 多选, 预览\n名字: wing"
        );
        assert!(!outcome.elicitation_unsupported);
        assert!(
            mock.permission_log().is_empty(),
            "有表单能力时不发 permission"
        );

        let requests = mock.seen_elicitations();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.message, "3 questions");
        let ElicitationMode::Form(form) = &request.mode else {
            panic!("expected a form elicitation, got {:?}", request.mode);
        };
        match &form.scope {
            agent_client_protocol::schema::v1::ElicitationScope::Session(scope) => {
                assert_eq!(scope.session_id.to_string(), "s1");
                assert_eq!(
                    scope.tool_call_id.as_ref().map(|id| id.to_string()),
                    Some("tc_ask".to_string()),
                    "表单挂在触发它的工具卡片上"
                );
            }
            other => panic!("expected a session-scoped elicitation, got {other:?}"),
        }
        assert_eq!(form.requested_schema.properties.len(), 3);
    }

    #[test]
    fn elicitation_answer_lines_and_placeholders() {
        let event = questions_ask();
        let questions = questions_of(&event);

        // 部分未答：缺失 / 空串 / 空白 / 类型不符 → 占位；多选顺序保持客户端给的顺序。
        let response = accepted(&[
            ("theme", json!("深色")),
            ("features", json!(["预览", "   ", "多选"])),
            ("name", json!("   ")),
        ]);
        assert_eq!(
            elicitation_answer(questions, &response),
            "配色: 深色\nfeatures: 预览, 多选\n名字: (user did not answer)"
        );

        let response = accepted(&[("theme", json!(3)), ("features", json!("裸字符串"))]);
        assert_eq!(
            elicitation_answer(questions, &response),
            "配色: (user did not answer)\nfeatures: 裸字符串\n名字: (user did not answer)",
            "类型不符按未答；单选题收到自由文本则原样当答案"
        );

        // 全部未答（accept 但空 content）。
        let response = CreateElicitationResponse::new(ElicitationAction::Accept(
            ElicitationAcceptAction::new(),
        ));
        assert_eq!(
            elicitation_answer(questions, &response),
            "配色: (user did not answer)\nfeatures: (user did not answer)\n名字: (user did not answer)"
        );

        // decline / cancel → 取消哨兵。
        for action in [ElicitationAction::Decline, ElicitationAction::Cancel] {
            let response = declined(action);
            assert_eq!(elicitation_answer(questions, &response), ASK_CANCEL_CONTENT);
        }
    }

    #[test]
    fn header_falls_back_to_the_question_id() {
        let event = questions_ask();
        let questions = questions_of(&event);
        // features 的 header 是空串 → 用 id。
        let response = accepted(&[("features", json!(["多选"]))]);
        let lines = elicitation_answer(questions, &response);
        assert!(lines.contains("features: 多选"), "{lines}");
    }

    // ---- ③ 回退 ----

    /// review r1 S-1 的最小复现形状：自由文本题在前、有选项的题在后。
    fn free_text_first_ask() -> WingEvent {
        event(json!({
            "type": "ask",
            "tool_call_id": "tc_ff",
            "questions": [
                {"id": "name", "header": "名字", "question": "叫什么名字"},
                {"id": "theme", "header": "配色", "question": "选一个配色",
                 "options": [{"label": "浅色"}, {"label": "深色"}]},
            ],
            "created_at": "c",
            "session_id": "s1",
            "request_id": "r",
        }))
    }

    /// 一份权限卡片是否满足 omnigent「选项卡」的门槛：≥2 条非空唯一 label + 含 `reject_*`。
    fn looks_like_a_choice_card(options: &[PermissionOption]) -> bool {
        let labels: Vec<&str> = options.iter().map(|option| option.name.as_str()).collect();
        options.len() >= 2
            && labels.iter().all(|label| !label.trim().is_empty())
            && labels
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == labels.len()
            && options
                .iter()
                .any(|option| matches!(option.kind, PermissionOptionKind::RejectOnce))
    }

    #[tokio::test]
    async fn fallback_when_the_client_has_no_elicitation() {
        let event = questions_ask();
        let questions = questions_of(&event);
        let mock = ScriptedAsk::ready_permissions([
            Ok(selected(&option_id_for(0, 0))),
            Ok(selected(SKIP_OPTION_ID)),
            Ok(selected(CONTINUE_OPTION_ID)),
        ]);

        let outcome = resolve_with(
            &mock,
            false,
            &session(),
            "tc_ask",
            AskView::Questions(questions),
        )
        .await;
        assert_eq!(
            outcome.answer,
            "配色: 浅色\nfeatures: (user did not answer)\n名字: (user did not answer)"
        );
        assert!(!outcome.elicitation_unsupported);
        assert!(
            mock.seen_elicitations().is_empty(),
            "无能力不发 elicitation"
        );

        // 逐题卡片：合成 id、标题 = 问题全文、选项 = 保留 id(allow_once) + 逃生选项。
        let requests = mock.seen_permissions();
        assert_eq!(
            mock.permission_log(),
            vec![
                "permission:tc_ask:theme",
                "permission:tc_ask:features",
                "permission:tc_ask:name"
            ],
            "逐题串行、按题目顺序"
        );
        assert_eq!(
            requests[0].tool_call.fields.title.as_deref(),
            Some("选一个配色")
        );
        assert_eq!(requests[0].tool_call.fields.kind, Some(ToolKind::Other));
        let first: Vec<String> = requests[0]
            .options
            .iter()
            .map(|option| option.option_id.to_string())
            .collect();
        assert_eq!(
            first,
            vec![
                option_id_for(0, 0),
                option_id_for(0, 1),
                SKIP_OPTION_ID.to_string()
            ],
            "optionId 用保留前缀 + 下标，不拿 label 当 id（N-4）"
        );
        assert_eq!(
            requests[0]
                .options
                .iter()
                .map(|option| option.name.as_str())
                .collect::<Vec<_>>(),
            vec!["浅色", "深色", "Skip"],
            "label 仍原样展示"
        );
        assert_eq!(
            requests[0].options[2].kind,
            PermissionOptionKind::RejectOnce
        );

        // 自由文本题：Continue(allow_once) + Skip(reject_once)——两条都为「未答并继续」，
        // 且满足「≥2 + 含 reject」的选项卡门槛（N-5/S-1）。
        let free_form = &requests[2].options;
        assert_eq!(
            free_form
                .iter()
                .map(|option| (
                    option.option_id.to_string(),
                    option.name.as_str(),
                    option.kind
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    CONTINUE_OPTION_ID.to_string(),
                    "Continue",
                    PermissionOptionKind::AllowOnce
                ),
                (
                    SKIP_OPTION_ID.to_string(),
                    "Skip",
                    PermissionOptionKind::RejectOnce
                ),
            ]
        );
        assert!(looks_like_a_choice_card(free_form));
        for request in &requests {
            assert!(
                looks_like_a_choice_card(&request.options),
                "每张卡都该是可渲染的选项卡：{:?}",
                request.options
            );
        }
    }

    /// S-1 回归：自由文本题的正面作答（Approve / Continue）不再吞掉后续题目。
    #[tokio::test]
    async fn a_free_text_question_keeps_asking_after_a_positive_answer() {
        let ask = free_text_first_ask();
        let questions = questions_of(&ask);
        // 「同意」的两条路（Continue / Skip）都必须继续问下一题。
        for positive in [CONTINUE_OPTION_ID, SKIP_OPTION_ID] {
            let mock = ScriptedAsk::ready_permissions([
                Ok(selected(positive)),
                Ok(selected(&option_id_for(1, 1))),
            ]);
            let outcome = resolve_with(
                &mock,
                false,
                &session(),
                "tc_ff",
                AskView::Questions(questions),
            )
            .await;
            assert_eq!(
                outcome.answer, "名字: (user did not answer)\n配色: 深色",
                "自由文本题之后的题目必须照常询问（{positive}）"
            );
            assert_eq!(
                mock.permission_log(),
                vec!["permission:tc_ff:name", "permission:tc_ff:theme"],
                "第二题必须被发出（{positive}）"
            );
        }

        // 对照：只有真正的 `cancelled` outcome（轮次取消）才停。
        let mock = ScriptedAsk::ready_permissions([Ok(permission_cancelled())]);
        let outcome = resolve_with(
            &mock,
            false,
            &session(),
            "tc_ff",
            AskView::Questions(questions),
        )
        .await;
        assert_eq!(
            outcome.answer,
            "名字: (user did not answer)\n配色: (user did not answer)"
        );
        assert_eq!(mock.permission_log(), vec!["permission:tc_ff:name"]);
    }

    #[tokio::test]
    async fn fallback_requests_are_serial() {
        let (release, released) = oneshot::channel();
        let mock = std::sync::Arc::new(ScriptedAsk::new());
        mock.push_deferred(released);
        // 第二题（features，多选）的「多选」是第 0 个选项 → 保留 id；第三题走 Continue。
        mock.push_permissions([
            Ok(selected(&option_id_for(1, 0))),
            Ok(selected(CONTINUE_OPTION_ID)),
        ]);

        let flow_mock = std::sync::Arc::clone(&mock);
        let flow = tokio::spawn(async move {
            let event = questions_ask();
            resolve_with(
                &*flow_mock,
                false,
                &session(),
                "tc_ask",
                AskView::Questions(questions_of(&event)),
            )
            .await
        });

        // 第一题的卡片发出之后：第二题必须等第一题答完——请求日志里只有第一条。
        wait_for_request(&mock, "permission:tc_ask:theme").await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            mock.permission_log(),
            vec!["permission:tc_ask:theme"],
            "第一题未作答前不得发第二题"
        );

        release
            .send(Ok(selected(&option_id_for(0, 0))))
            .expect("release the first question");
        let outcome = flow.await.expect("the ask flow finishes");
        assert_eq!(
            mock.permission_log(),
            vec![
                "permission:tc_ask:theme",
                "permission:tc_ask:features",
                "permission:tc_ask:name"
            ]
        );
        assert_eq!(
            outcome.answer,
            "配色: 浅色\nfeatures: 多选\n名字: (user did not answer)"
        );
    }

    /// 等到 mock 的请求日志里出现某条记录（有界轮询，避免依赖调度顺序）。
    async fn wait_for_request(mock: &ScriptedAsk, needle: &str) {
        for _ in 0..200 {
            if mock.permission_log().iter().any(|entry| entry == needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("request {needle} never reached the mock");
    }

    #[tokio::test]
    async fn fallback_permission_errors_leave_the_question_unanswered() {
        let event = questions_ask();
        let questions = questions_of(&event);
        // ① 请求失败、② 正常选中、③ 回了个我们没提供的 optionId（客户端乱来）。
        let mock = ScriptedAsk::ready_permissions([
            Err(Error::internal_error().data("boom")),
            Ok(selected(&option_id_for(1, 1))),
            Ok(selected("从未提供过的选项")),
        ]);
        let outcome = resolve_with(
            &mock,
            false,
            &session(),
            "tc_ask",
            AskView::Questions(questions),
        )
        .await;
        assert_eq!(
            outcome.answer,
            "配色: (user did not answer)\nfeatures: 预览\n名字: (user did not answer)",
            "单题失败/未知选项都不中断后续题目"
        );
        assert_eq!(mock.permission_log().len(), 3, "三题都问到了");
    }

    /// `cancelled` outcome = 轮次已取消：不再追问后续题目（剩余按未答收口）。
    #[tokio::test]
    async fn a_cancelled_outcome_stops_the_remaining_questions() {
        let event = questions_ask();
        let questions = questions_of(&event);
        let mock = ScriptedAsk::ready_permissions([Ok(permission_cancelled())]);
        let outcome = resolve_with(
            &mock,
            false,
            &session(),
            "tc_ask",
            AskView::Questions(questions),
        )
        .await;
        assert_eq!(
            outcome.answer,
            "配色: (user did not answer)\nfeatures: (user did not answer)\n名字: (user did not answer)"
        );
        assert_eq!(
            mock.permission_log(),
            vec!["permission:tc_ask:theme"],
            "轮次取消后不得再发后面的卡片"
        );
    }

    // ---- 能力门控与 `-32601` 降级 ----

    #[tokio::test]
    async fn method_not_found_downgrades_and_falls_back() {
        let event = questions_ask();
        let questions = questions_of(&event);
        let mock = ScriptedAsk::new();
        mock.push_elicitation(Err(Error::method_not_found()));
        mock.push_permissions([
            Ok(selected(&option_id_for(0, 1))),
            Ok(selected(SKIP_OPTION_ID)),
            Ok(selected(SKIP_OPTION_ID)),
        ]);

        let outcome = resolve_with(
            &mock,
            true,
            &session(),
            "tc_ask",
            AskView::Questions(questions),
        )
        .await;
        assert!(
            outcome.elicitation_unsupported,
            "`-32601` 必须把降级意图交回调用方（粘性）"
        );
        assert_eq!(
            outcome.answer,
            "配色: 深色\nfeatures: (user did not answer)\n名字: (user did not answer)"
        );
        assert_eq!(mock.elicitation_log().len(), 1, "只试探一次");
        assert_eq!(mock.permission_log().len(), 3, "随后逐题走回退");
    }

    #[tokio::test]
    async fn other_elicitation_errors_fall_back_without_downgrading() {
        let event = questions_ask();
        let questions = questions_of(&event);
        let mock = ScriptedAsk::new();
        mock.push_elicitation(Err(Error::internal_error().data("transient")));
        mock.push_permissions([
            Ok(selected(&option_id_for(0, 0))),
            Ok(selected(SKIP_OPTION_ID)),
            Ok(selected(SKIP_OPTION_ID)),
        ]);

        let outcome = resolve_with(
            &mock,
            true,
            &session(),
            "tc_ask",
            AskView::Questions(questions),
        )
        .await;
        assert!(
            !outcome.elicitation_unsupported,
            "瞬时错误不降级：下次仍可试表单"
        );
        assert_eq!(mock.permission_log().len(), 3);
    }
}
