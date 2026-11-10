# wing/context/compaction.py
"""Compactor — 上下文压缩引擎。

负责判断压缩阈值、计算切割点、执行 LLM 压缩调用。
不管理消息链状态——那是 ContextManager 的职责。
"""

import re
from dataclasses import dataclass, field
from datetime import datetime
from typing import TYPE_CHECKING

from wing.provider.base import ModelProvider
from wing.schema import LLMResponse, LLMUsage, Message, Tool

if TYPE_CHECKING:
    from wing.provider.base import RequestOptions
    from wing.schema import CachePart


@dataclass
class LLMMessagesResult:
    """get_messages_for_llm() 的结构化返回值。

    tools: 本次 LLM 调用应使用的工具列表（声明集快照）。
    Agent 直接将此传给 provider.generate(tools=...)。

    cache_parts: system 侧的分段（缓存账本探针的输入，契约 §1）。由 CM 产出、经
    ``RequestOptions.cache_parts`` 交给 provider 与 wire 侧分段合并成完整段列表。
    无探针时为空元组（不产生任何额外计算）。
    """

    messages: list[Message]
    tools: list[Tool]
    cache_parts: tuple["CachePart", ...] = ()


@dataclass
class PendingCompact:
    """预计算的压缩结果，等待 apply 到消息链。

    start_uuid / end_uuid 用于 UUID 自校验：apply 时在当前活跃链中查找，
    找不到则说明链已被修改（rewind / 手动 compact），自动丢弃。
    """

    compact_content: str
    start_uuid: str
    end_uuid: str
    usage: LLMUsage | None = None
    timestamp: str = field(default_factory=lambda: datetime.now().isoformat())


COMPACT_IMAGE_MARKER = "[image message]"
"""纯图消息（content 为空、媒体被剥离）在**压缩请求**里的替代文本。

压缩请求剥离媒体引用（摘要目标是文本），但不能因此出现空 content 的 user 消息
——部分 OpenAI 兼容网关对空 content 直接 400，压缩整轮失败（长会话里图片一多
就再也压不动）。短标记保留"该位置有一条只有图片的用户消息"这一事实，原链消息
零改动。
"""


def _for_summary(message: Message) -> Message:
    """把一条消息投影进压缩请求：剥离媒体引用；纯图消息换成短标记。

    无媒体的消息原样返回（同一个对象——请求路径只读）；有媒体的浅拷贝，
    原链消息（含 uuid / media）零改动。``content`` 为空且有 media 的消息
    （user 贴图；正常链路 tool 消息恒有文本）重建为等价消息：正文换成短标记、
    保留 role 与 tool_call_id（tool 消息在两条协议里都必须携带它）。
    """
    if not message.media:
        return message
    if message.content:
        return message.model_copy(update={"media": None})
    return Message(
        role=message.role,
        content=COMPACT_IMAGE_MARKER,
        tool_call_id=message.tool_call_id,
    )


def resolve_context_tokens(
    messages: list[Message], server_tokens: int | None = None
) -> int:
    """当前上下文的 token 数：服务端回传的 prompt_tokens（非零）优先，否则本地估算。

    这是**唯一**口径：压缩阈值与 `context_stats` 的显示数字共用它，否则会出现
    "面板显示没超、却触发了压缩"。
    """
    if server_tokens is not None and server_tokens > 0:
        return server_tokens
    return sum(m.estimate_tokens() for m in messages)


def _is_request_boundary(messages: list[Message], idx: int) -> bool:
    """``idx`` 是不是"切在它之前"合法、且上下文大小已知的位置。

    - ``user`` 消息：新 turn 的起点；
    - ``assistant`` 消息：请求的应答位置或续跑位置——保留区以 assistant 开头合法
      （两条协议都接受相邻 assistant / 由序列化层合并），以 ``tool`` 开头则会让
      tool_result 变成孤儿，所以 ``tool`` / ``system`` 永远不是候选。
    """
    return messages[idx].role in ("user", "assistant")


def select_cut(
    messages: list[Message],
    *,
    trust_start: int,
    total_tokens: int,
    keep_recent_tokens: int,
) -> int:
    """按**服务端 usage** 选压缩切点：返回保留区起点下标（0 = 无可压）。

    每个带 usage 的 assistant 消息的 ``prompt_tokens`` 就是"该请求开始时上下文有
    多大"，所以切点只能落在**上下文大小已知、且不破坏配对**的位置上：``user`` 消息
    与 ``assistant`` 消息（保留区以 assistant 开头合法，绝不以 ``tool`` 开头——那会
    让 tool_result 变成孤儿）。工具调用对因此构造性闭合：切点左侧的下一个请求发出
    时，上一组 tool_result 必然已齐。

    ``p(c)`` = c 及其之后第一条有 usage 的 assistant 的 ``prompt_tokens``（c 位于最后
    一条 assistant 之后时就是 ``total`` 本身——末尾那条还没被回答的 user 消息）。
    取**最早**的满足 ``p(c) >= total - keep_recent`` 的候选：``total - p(c)`` 即 c
    之后那段上下文的大小（system 与 tools 在差值里抵消），保留区因此"尽量大、
    但不超过 keep_recent"。``user`` 边界与它后面那条 assistant 的 ``p`` 相同，因此
    "整轮保留"总是优先于"保留答案、把问题压进摘要"——后者只在轮首 message 无法作为
    切点（窗口第一条消息）时才会发生。

    本地估算在这条路径上一个数字都不参与——"总是估得太低、切得太深"正是旧实现拿
    "只含 message 的估算"去对"含 system + tools 的窗口阈值"的结果。

    Args:
        messages: 上下文窗口（``ContextManager.get_context_window()``）。
        trust_start: 窗口里服务端 usage 可信的起点（``ContextManager`` 的信任边界；
            压缩换入 / rewind 之前的 usage 度量的是已经不存在的窗口）。
        total_tokens: 当前上下文大小（信任区间里最后一条 usage）。
        keep_recent_tokens: 目标保留区大小（模型声明）。
    """
    if not messages or total_tokens <= 0:
        return 0
    target = total_tokens - keep_recent_tokens
    # 反向扫描：cursor 恒为"c 处上下文有多大"——c 之及之后第一条有 usage 的
    # assistant 的 prompt_tokens；c 在最后一条 assistant 之后（末尾那条还没被回答的
    # user 消息）时就是 total 本身。它沿链向左单调不减，因此第一个 p(c) < target
    # 的候选就说明再往前都不合格。
    cursor = total_tokens
    choice = 0
    for idx in range(len(messages) - 1, -1, -1):
        if idx < trust_start:
            break
        msg = messages[idx]
        if (
            msg.role == "assistant"
            and msg.usage is not None
            and msg.usage.prompt_tokens > 0
        ):
            cursor = msg.usage.prompt_tokens
        if idx == 0 or not _is_request_boundary(messages, idx):
            continue
        if cursor < target:
            break
        choice = idx
    return choice


class Compactor:
    """上下文压缩引擎。

    双阈值模型：
      - compact_window_tokens = context_window_tokens - keep_recent_tokens
      - Early trigger: tokens >= compact_window_tokens → 后台异步压缩
      - Apply:         tokens >= context_window_tokens → 换入预计算结果

    切点见模块级 `select_cut`（服务端 usage 锚定）与 `_calc_cut_idx`（无服务端
    数字时的估算兜底）。
    """

    # 本 PROMPT 取自 https://github.com/browser-use/agent-sdk（五段式续作摘要模板）。
    _COMPACT_PROMPT_BODY = """You have been working on the task described above but have not yet completed it. Write a continuation summary that will allow you (or another instance of yourself) to resume work efficiently in a future context window where the conversation history will be replaced with this summary. Your summary should be structured, concise, and actionable. Include:

    1. Task Overview
    The user's core request and success criteria
    Any clarifications or constraints they specified

    2. Current State
    What has been completed so far
    Files created, modified, or analyzed (with paths if relevant)
    Key outputs or artifacts produced

    3. Important Discoveries
    Technical constraints or requirements uncovered
    Decisions made and their rationale
    Errors encountered and how they resolved
    What approaches were tried that didn't work (and why)

    4. Next Steps
    Specific actions needed to complete the task
    Any blockers or open questions to resolve
    Priority order if multiple steps remain

    5. Context to Preserve
    User preferences or style requirements
    Domain-specific details that aren't obvious
    Any promises made to the user

    Be concise but complete - err on the side of including information that would prevent duplicate work or repeated mistakes. Write in a way that enables immediate resumption of the task."""

    # 格式约束尾部：<summary> 标签 + CRITICAL 工具禁令。拆分目的是让用户指令
    # （/compact <侧重>）条件渲染地插在主体与格式约束之间——对提示词只做增加、
    # 不做修改，无指令路径（后台自动压缩、裸手动压缩）的 prompt 与历史
    # COMPACT_PROMPT 逐字节一致。
    # 指令必须排在格式约束之前、CRITICAL 保持末位 recency：
    # _extract_compact_result 以正则强提取 <summary> 标签，格式失守
    # 即整次压缩报废（手动 500 / 后台静默丢弃）。
    _COMPACT_PROMPT_FORMAT = """

    Wrap your summary in <summary></summary> tags.

    ## CRITICAL: DO NOT call any tools or functions.
    ## DO NOT execute any tool calls.
    ## Your entire response must be ONLY the summary wrapped in <summary> tags.
    ## Any tool calls in your response will be SILENTLY IGNORED."""

    # 兼容别名：恒等于 BODY + FORMAT（测试锁定恒等式，防止后续改动
    # 其一导致带指令/不带指令两条路径的 prompt 漂移）。
    COMPACT_PROMPT = _COMPACT_PROMPT_BODY + _COMPACT_PROMPT_FORMAT

    def __init__(
        self,
        context_window_tokens: int,
        keep_recent_tokens: int,
    ) -> None:
        self.set_window(context_window_tokens, keep_recent_tokens)

    def set_window(self, context_window_tokens: int, keep_recent_tokens: int) -> None:
        """设置双阈值（构造期与运行期切换模型共用）。

        先校验后赋值：非法组合 raise 且状态零变化（调用方的切换不留半截）。
        """
        if context_window_tokens <= keep_recent_tokens:
            raise ValueError("context_window_tokens must > keep_recent_tokens")
        self.context_window_tokens = context_window_tokens
        self.keep_recent_tokens = keep_recent_tokens
        self.compact_window_tokens = context_window_tokens - keep_recent_tokens

    def copy(self) -> "Compactor":
        """独立副本——每个会话持有自己的阈值（换模型只改本会话）。"""
        return Compactor(self.context_window_tokens, self.keep_recent_tokens)

    # ── 阈值判断 ────────────────────────────────

    def need_early_trigger(
        self, messages: list[Message], server_tokens: int | None = None
    ) -> bool:
        """判断是否应启动后台异步压缩。

        当 tokens >= compact_window_tokens 时触发。
        """
        return (
            resolve_context_tokens(messages, server_tokens)
            >= self.compact_window_tokens
        )

    def need_apply_compact(
        self, messages: list[Message], server_tokens: int | None = None
    ) -> bool:
        """判断是否应将预计算的压缩结果换入消息链。

        当 tokens >= context_window_tokens 时触发。
        """
        return (
            resolve_context_tokens(messages, server_tokens)
            >= self.context_window_tokens
        )

    # ── 压缩执行 ────────────────────────────────

    def _extract_compact_result(self, text: str) -> str:
        if match := re.search(r"<summary>(.*?)</summary>", text, re.DOTALL):
            return match.group(1).strip()
        raise RuntimeError("can't extract summary")

    def _calc_cut_idx(self, messages: list[Message]) -> int:
        """**兜底**切点：没有任何可信的服务端 usage 时才走这里。

        从头部开始累积本地估算的 token，超过 compact_window_tokens 时停止，然后通过
        `_find_safe_cut_idx` 补齐工具调用对。口径与 `need_early_trigger` 的估算兜底
        一致（两边都拿不到服务端数字时才同时生效），因此不会出现"触发用服务端、
        切点用估算"的混单位。已知偏差：message 估算不含 system + tools 声明
        （窗口阈值含），切点会偏深——这正是主路径改用 `select_cut` 的原因。
        """
        if not messages:
            return 0

        cut_idx = 0
        tokens = 0
        for i, msg in enumerate(messages):
            cut_idx = i + 1
            tokens += msg.estimate_tokens()
            if tokens >= self.compact_window_tokens:
                break

        return self._find_safe_cut_idx(messages, cut_idx)

    def _find_safe_cut_idx(self, messages: list[Message], cut_idx: int) -> int:
        """确保工具调用对完整：向后扩展直到所有 pending tool calls 闭合。"""
        # Phase 1: 扫描窗口内，收集未闭合的 tool calls
        pending = set()
        for msg in messages[:cut_idx]:
            if msg.role == "assistant" and msg.tool_calls:
                pending.update(tc.id for tc in msg.tool_calls)
            elif msg.role == "tool" and msg.tool_call_id in pending:
                pending.discard(msg.tool_call_id)

        if not pending:
            return cut_idx

        # Phase 2: 向后扩展，直到闭合所有 pending
        for i, msg in enumerate(messages[cut_idx:], start=cut_idx):
            if msg.role == "tool" and msg.tool_call_id in pending:
                pending.discard(msg.tool_call_id)
                if not pending:
                    return i + 1

        return len(messages)

    async def do_compact(
        self,
        full_messages: list[Message],
        model: str,
        model_provider: ModelProvider,
        tools: list | None = None,
        instruction: str | None = None,
        options: "RequestOptions | None" = None,
    ) -> LLMResponse:
        """执行压缩。

        full_messages 是完整上下文（system + head），
        与主 agent 调用 LLM 的前缀完全一致（含 tools），从而最大化缓存命中率。

        prompt（BODY [+ 指令块] + FORMAT）被追加为最后一条 user message。
        工具调用在响应中被静默忽略。

        options 是调用方注入的会话级参数（session id 保持与主调用同一
        prompt cache key；thinking / effort 开关与主调用同口径）——provider
        无状态化后不再有「实例自带会话状态」可言，compact 请求的缓存亲和
        与开关一致性全靠它。

        instruction 是用户通过 /compact <侧重> 下发的压缩侧重指令
        （仅手动压缩传入；后台自动压缩不带）。条件渲染：存在则插入
        BODY 与 FORMAT 之间——指令只声明 summary content 范围内的
        优先级，<summary> 标签 + CRITICAL 格式约束保持末位 recency
        （_extract_compact_result 强提取标签，格式失守即整次压缩报废）；
        不存在（含空白）时 prompt 与 COMPACT_PROMPT 逐字节一致，
        默认路径零影响。
        """
        instruction_block = ""
        if instruction and instruction.strip():
            instruction_block = (
                "\n\n    ## Additional instruction from the user "
                "(highest priority for summary content):\n    "
                f"{instruction.strip()}"
            )
        prompt = (
            self._COMPACT_PROMPT_BODY + instruction_block + self._COMPACT_PROMPT_FORMAT
        )
        # 压缩请求纯文本：剥离消息里的媒体引用（浅拷贝，原链消息零改动——
        # model_copy 不写 uuid/parent_uuid，也不携带 media 字节）。摘要目标是
        # 文本，连占位都不需要；图片引用留在原消息里，会话继续可用。
        # 纯图消息（content 为空、内容全在 media）不能以空 content 出现在请求里
        # （部分 OpenAI 兼容网关直接 400）——换成短标记（见 `_for_summary`）。
        request_messages = [_for_summary(m) for m in full_messages]
        messages = request_messages + [Message(role="user", content=prompt)]

        # 消费完整（非流式路径只产出一个响应）：不 break——被 break 放弃的
        # 生成器要到 finalizer 才收尾，provider 的在途计数（退场「排空即关」
        # 的依据）会白挂一程。
        response = None
        async for item in model_provider.generate(
            messages, model, tools=tools, stream=False, options=options
        ):
            response = item

        if response is None:
            response = LLMResponse(content="")

        summary = self._extract_compact_result(response.content or "")

        return LLMResponse(
            content=f"[Compact] {summary}",
            usage=response.usage,
            reasoning_content=response.reasoning_content,
        )
