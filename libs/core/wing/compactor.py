# wing/compactor.py
"""Compactor — 上下文压缩引擎。

负责判断压缩阈值、计算切割点、执行 LLM 压缩调用。
不管理消息链状态——那是 ContextManager 的职责。
"""

import re

from wing.provider.base import ModelProvider

from .schema import LLMResponse, Message


class Compactor:
    """上下文压缩引擎。

    双阈值模型：
      - compact_window_tokens = context_window_tokens - keep_recent_tokens
      - Early trigger: tokens >= compact_window_tokens → 后台异步压缩
      - Apply:         tokens >= context_window_tokens → 换入预计算结果
    """

    # This PROMPT comes from https://github.com/browser-use/agent-sdk
    # 内容主体：五段式续作摘要模板（截至 "immediate resumption" 段）。
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

    # 格式约束尾部：<summary> 标签 + CRITICAL 工具禁令。
    # 拆分目的：用户指令（/compact <侧重>）以条件渲染方式插在主体与
    # 格式约束之间（存在则插入、不存在则原样）——对提示词只做增加、
    # 不做修改，无指令路径（后台自动压缩、裸手动压缩）的 prompt 与
    # 历史 COMPACT_PROMPT 逐字节一致。
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
        if context_window_tokens <= keep_recent_tokens:
            raise ValueError("context_window_tokens must > keep_recent_tokens")
        self.context_window_tokens = context_window_tokens
        self.keep_recent_tokens = keep_recent_tokens
        self.compact_window_tokens = context_window_tokens - keep_recent_tokens

    # ── Token 估算 ──────────────────────────────

    def _estimate_tokens(self, messages: list[Message]) -> int:
        return sum(m.estimate_tokens() for m in messages)

    # ── 阈值判断 ────────────────────────────────

    def _resolve_tokens(
        self, messages: list[Message], server_tokens: int | None = None
    ) -> int:
        """优先使用服务端回传的 prompt_tokens，fallback 到本地估算。"""
        if server_tokens is not None and server_tokens > 0:
            return server_tokens
        return self._estimate_tokens(messages)

    def need_early_trigger(
        self, messages: list[Message], server_tokens: int | None = None
    ) -> bool:
        """判断是否应启动后台异步压缩。

        当 tokens >= compact_window_tokens 时触发。
        """
        return (
            self._resolve_tokens(messages, server_tokens) >= self.compact_window_tokens
        )

    def need_apply_compact(
        self, messages: list[Message], server_tokens: int | None = None
    ) -> bool:
        """判断是否应将预计算的压缩结果换入消息链。

        当 tokens >= context_window_tokens 时触发。
        """
        return (
            self._resolve_tokens(messages, server_tokens) >= self.context_window_tokens
        )

    # ── 压缩执行 ────────────────────────────────

    def _extract_compact_result(self, text: str) -> str:
        if match := re.search(r"<summary>(.*?)</summary>", text, re.DOTALL):
            return match.group(1).strip()
        raise RuntimeError("can't extract summary")

    def _calc_cut_idx(self, messages: list[Message]) -> int:
        """计算要压缩的消息切割点。

        从头部开始累积 token，超过 compact_window_tokens 时停止。
        然后通过 _find_safe_cut_idx 确保工具调用对完整。
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
    ) -> LLMResponse:
        """执行压缩。

        full_messages 是完整上下文（system + head），
        与主 agent 调用 LLM 的前缀完全一致（含 tools），从而最大化缓存命中率。

        prompt（BODY [+ 指令块] + FORMAT）被追加为最后一条 user message。
        工具调用在响应中被静默忽略。

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
        messages = full_messages + [Message(role="user", content=prompt)]

        response = None
        async for item in model_provider.generate(
            messages, model, tools=tools, stream=False
        ):
            response = item
            break

        if response is None:
            response = LLMResponse(content="")

        summary = self._extract_compact_result(response.content or "")

        return LLMResponse(
            content=f"[Compact] {summary}",
            usage=response.usage,
            reasoning_content=response.reasoning_content,
        )
