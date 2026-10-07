//! Ask 映射（03 步）：wing 的两类 `ask` 事件 → ACP 的正式交互面。
//!
//! 三径分流（形态判据 = `Ask` 事件的字段，沿用 01 的占位实现）：
//!
//! | 形态 | 出路 |
//! |------|------|
//! | `required` + `choices`（Bash 危险命令确认，retired 形态） | `session/request_permission`：y=allow_once / yolo=allow_always / n=reject_once |
//! | `questions`（AskUserQuestion，1–4 题） | `elicitation/create`（form 模式，**能力门控**） |
//! | 同上但客户端无 elicitation 能力（或回 `-32601`） | 回退：逐题串行 `session/request_permission` |
//!
//! 答案经 WS `ClientRequest{content, tool_call_id}` 定向回写 feedback waiter，内容格式是
//! **逐字契约**（常量直接复用 TUI 面板的 `shared::panels::ask`，见 [`AnswerLines`] 的说明）：
//!
//! - `questions` 形态：每题一行 `header: answer`（header 空回退 id）；多选 label 以 `", "`
//!   连接；未答 `(user did not answer)`；取消 `__wing_ask_cancelled__`；
//! - Bash 形态：**裸 token**（`y` / `n` / `yolo`）——后端 `_parse_feedback` 只认这三个，
//!   取消哨兵会被判成无效选项并**反复追问**（等价于把轮次挂到 6000s 超时）。
//!
//! 并发语义（design D5）：ask 处理**就地 await** 在轮次事件循环里——同一会话至多一个在途
//! agent→client 请求，后续 ask 留在会话事件缓冲里排队（与 TUI `ask_panels` 的 FIFO 队列
//! 同语义）。因此答案永远在轮次内回写，不需要 detached 任务的簿记，也不存在「终态之后才到
//! 的答案」竞态。
//!
//! 可测性：整条流程（[`resolve_with`]）只依赖 [`AskInteraction`] 这一小片交互面，
//! 单测用脚本化 mock 驱动，不需要真实连接。
//!
//! [`AnswerLines`]: #构造答案

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

/// 回退路径里「跳过本题」选项的 `optionId`。
///
/// 用连字符包起来的保留名：后端保证题目标签非空且互不相同（`ask_user.py` 校验），
/// 不会与 label 撞车，客户端把它原样回给我们即可识别。
const SKIP_OPTION_ID: &str = "__wing_skip__";

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
    /// Bash 危险命令确认（retired 形态）：`questions` 空 + `required` + `choices` 非空。
    RequiredChoice,
    /// 未分类形态（当前后端不会产生）：防御性回退（回取消哨兵，见 [`resolve_with`]）。
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
        AskView::RequiredChoice
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
        AskView::RequiredChoice => {
            AskOutcome::answer(bash_confirmation(interaction, session_id, tool_call_id).await)
        }
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

// ============================================================
// ① Bash 危险命令确认 → permission
// ============================================================

/// Bash 确认：`session/request_permission`，三选项的 `optionId` 就是回写 token。
async fn bash_confirmation(
    interaction: &impl AskInteraction,
    session_id: &SessionId,
    tool_call_id: &str,
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
        bash_options(),
    );
    match interaction.permission(request).await {
        Ok(response) => bash_token(&response.outcome),
        Err(err) => {
            tracing::warn!(
                tool_call_id,
                error = %err,
                "acp: bash confirmation request failed; rejecting"
            );
            BASH_TOKEN_N.to_string()
        }
    }
}

/// 三个选项：`optionId` 即回写 token，客户端原样回传，映射恒等。
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

/// permission 结果 → 回写 token。
///
/// `cancelled` outcome、未知 `optionId`、请求出错一律回 [`BASH_TOKEN_N`]（拒绝）：
/// 后端只认 y/n/yolo，**绝不能**用取消哨兵（`_parse_feedback` 会判无效并重新追问）。
fn bash_token(outcome: &RequestPermissionOutcome) -> String {
    let RequestPermissionOutcome::Selected(selected) = outcome else {
        return BASH_TOKEN_N.to_string();
    };
    match selected.option_id.0.as_ref() {
        BASH_TOKEN_Y => BASH_TOKEN_Y.to_string(),
        BASH_TOKEN_YOLO => BASH_TOKEN_YOLO.to_string(),
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
async fn fallback_questions(
    interaction: &impl AskInteraction,
    session_id: &SessionId,
    tool_call_id: &str,
    questions: &[AskQuestion],
) -> String {
    let mut answers: Vec<String> = Vec::with_capacity(questions.len());
    for question in questions {
        let Some(answer) = fallback_question(interaction, session_id, tool_call_id, question).await
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
        fallback_options(question),
    );
    match interaction.permission(request).await {
        Ok(response) => fallback_choice_answer(question, &response.outcome),
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

/// 该题的选项（每条 `allow_once`，`optionId` = label，客户端原样回传）+ 一条 `Skip`。
///
/// 带 `reject_*` 的 ≥2 选项是 omnigent 走「选项卡」而不是 Approve/Reject 二态的条件；
/// 自由文本题只剩 Skip 一条 → 客户端无论如何作答都落到「未答」（权限卡片收不到自由文本）。
fn fallback_options(question: &AskQuestion) -> Vec<PermissionOption> {
    let mut options: Vec<PermissionOption> = enum_options(question)
        .into_iter()
        .map(|option| {
            PermissionOption::new(
                option.value.clone(),
                option.value,
                PermissionOptionKind::AllowOnce,
            )
        })
        .collect();
    options.push(PermissionOption::new(
        SKIP_OPTION_ID,
        "Skip",
        PermissionOptionKind::RejectOnce,
    ));
    options
}

/// 选中某个 label → 该 label 即答案；Skip / 未知 id → 未答占位；
/// `None` = `cancelled` outcome（轮次已取消，见 [`fallback_questions`]）。
fn fallback_choice_answer(
    question: &AskQuestion,
    outcome: &RequestPermissionOutcome,
) -> Option<String> {
    let RequestPermissionOutcome::Selected(selected) = outcome else {
        return None;
    };
    let chosen = selected.option_id.0.as_ref();
    if chosen == SKIP_OPTION_ID {
        return Some(UNANSWERED_PLACEHOLDER.to_string());
    }
    Some(
        enum_options(question)
            .into_iter()
            .find(|option| option.value == chosen)
            .map(|option| option.value)
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
        assert_eq!(view, AskView::RequiredChoice);

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

    // ---- ① Bash 三选项 ----

    #[tokio::test]
    async fn bash_confirmation_maps_three_tokens() {
        for (reply, expected) in [
            (selected("y"), "y"),
            (selected("yolo"), "yolo"),
            (selected("n"), "n"),
            (selected("surprise"), "n"),
            (permission_cancelled(), "n"),
        ] {
            let mock = ScriptedAsk::ready_permissions([Ok(reply)]);
            let outcome =
                resolve_with(&mock, true, &session(), "tc_bash", AskView::RequiredChoice).await;
            assert_eq!(outcome.answer, expected);
            assert!(!outcome.elicitation_unsupported);
        }
    }

    #[tokio::test]
    async fn bash_confirmation_request_shape() {
        let mock = ScriptedAsk::ready_permissions([Ok(selected("y"))]);
        resolve_with(&mock, true, &session(), "tc_bash", AskView::RequiredChoice).await;

        let requests = mock.seen_permissions();
        assert_eq!(requests.len(), 1, "只发一条 permission");
        let request = &requests[0];
        assert_eq!(request.session_id.to_string(), "s1");
        assert_eq!(request.tool_call.tool_call_id.to_string(), "tc_bash");
        assert_eq!(request.tool_call.fields.kind, Some(ToolKind::Execute));
        assert!(
            request.tool_call.fields.title.is_none(),
            "卡片已存在，不覆盖标题"
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
        let mock = ScriptedAsk::ready_permissions([Err(Error::internal_error().data("boom"))]);
        let outcome =
            resolve_with(&mock, true, &session(), "tc_bash", AskView::RequiredChoice).await;
        assert_eq!(outcome.answer, "n");
        assert!(!outcome.elicitation_unsupported);
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

    #[tokio::test]
    async fn fallback_when_the_client_has_no_elicitation() {
        let event = questions_ask();
        let questions = questions_of(&event);
        let mock = ScriptedAsk::ready_permissions([
            Ok(selected("浅色")),
            Ok(selected(SKIP_OPTION_ID)),
            Ok(permission_cancelled()),
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

        // 逐题卡片：合成 id、标题 = 问题全文、选项 = label(allow_once) + Skip(reject_once)。
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
        assert_eq!(first, vec!["浅色", "深色", SKIP_OPTION_ID]);
        assert_eq!(
            requests[0].options[2].kind,
            PermissionOptionKind::RejectOnce
        );
        let free_form: Vec<String> = requests[2]
            .options
            .iter()
            .map(|option| option.option_id.to_string())
            .collect();
        assert_eq!(free_form, vec![SKIP_OPTION_ID], "自由文本题只剩 Skip");
    }

    #[tokio::test]
    async fn fallback_requests_are_serial() {
        let (release, released) = oneshot::channel();
        let mock = std::sync::Arc::new(ScriptedAsk::new());
        mock.push_deferred(released);
        mock.push_permissions([Ok(selected("多选")), Ok(selected(SKIP_OPTION_ID))]);

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
            .send(Ok(selected("浅色")))
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
            Ok(selected("预览")),
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
            Ok(selected("深色")),
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
            Ok(selected("浅色")),
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
