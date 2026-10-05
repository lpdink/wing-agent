# wing/provider/anthropic/provider.py
"""Anthropic 协议 Provider — 基于 httpx 裸写。

处理 Anthropic Messages API 的结构差异：
- system prompt 为顶层字段
- content 为 block 数组（text / tool_use / tool_result / thinking）
- 流式事件为 content_block_start/delta/stop + message_delta
- 认证用 x-api-key header（非 Bearer）

实现按子包内三层切分：请求期消息序列化在 ``serialize``，流状态机与事件处理器在
``stream``；本模块承载 provider 对象（生命周期 / 公共 API / 状态 / 请求构造 / 泵）。
"""

from __future__ import annotations

import copy

import asyncio
import time
from typing import TYPE_CHECKING, AsyncIterator

import httpx

from wing.common.logger import log
from wing.common.with_retry import with_retry
from wing.provider.anthropic.serialize import _SerializeMixin
from wing.provider.anthropic.stream import (
    _StreamMixin,
    _StreamState,
    _is_zero_info_block,
)
from wing.provider.base import (
    ModelProvider,
    PendingToolView,
    StreamAccumulator,
)
from wing.provider.transport import (
    STREAM_IDLE_TIMEOUT,
    ProviderStreamError,
    lines_with_idle_timeout,
    make_http_timeout,
    parse_json_event,
    parse_sse_stream,
    raise_with_body,
)
from wing.schema import (
    LLMResponse,
    LLMUsage,
    Message,
    TextBlock,
    ThinkingBlock,
    Tool,
    ToolCall,
    ToolUseBlock,
)

if TYPE_CHECKING:
    from wing.config import ProviderConfig
    from wing.media import MediaAccess

# 运行时开启 thinking 且用户未配置 budget 时的默认预算。
# Anthropic 要求 type=enabled 必带 budget_tokens（1024 <= budget < max_tokens）。
_DEFAULT_THINKING_BUDGET = 4096


class AnthropicProvider(_SerializeMixin, _StreamMixin, ModelProvider):
    """Anthropic Messages API provider（httpx 实现）。"""

    def __init__(
        self,
        config: ProviderConfig,
        session_id: str | None = None,
        media: MediaAccess | None = None,
    ) -> None:
        self._config = config
        self._session_id = session_id
        # 会话媒体池（本步骤只持有，序列化在后续步骤接线）。
        self._media = media
        self.base_url = config.base_url.rstrip("/")
        self.reasoning_effort: str | None = config.reasoning_effort
        self.timeout_first_chunk = config.timeout_first_chunk
        self.timeout_total = config.timeout_total
        self.explicit_cache_mode = config.explicit_cache_mode
        # 深拷贝：运行时开关（set_thinking）会改写嵌套的 thinking dict，
        # 浅拷贝会把改动写穿到全局 ProviderConfig（污染其他会话/进程内重建）。
        self._extra_body: dict = copy.deepcopy(config.extra_body)
        self._anthropic_version = config.anthropic_version

        headers = self._make_headers(config.api_key, self._anthropic_version)

        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            headers=headers,
            timeout=make_http_timeout(),
        )
        log.info(f"AnthropicProvider initialized: {self.base_url}")

    # ─── Public API ───────────────────────────────────────────────

    @with_retry()
    async def generate(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None = None,
        stream: bool = False,
        accumulator: StreamAccumulator | None = None,
    ) -> AsyncIterator[LLMResponse]:
        log.info(f"[BEGIN] anthropic call {model} with {len(messages)} stream:{stream}")
        body = self._build_body(messages, model, tools, stream)

        if stream:
            async for item in self._generate_stream(body, model, accumulator):
                yield item
        else:
            async for item in self._generate_sync(body, model):
                yield item

    def create_accumulator(self) -> StreamAccumulator:
        return StreamAccumulator()

    def snapshot_blocks(self, accumulator: StreamAccumulator | None) -> list | None:
        """从累积状态提取已生成块——中断补提交路径。

        text/thinking 任意长度保留；未被 content_block_stop 终结的 tool
        块丢弃（半截参数不可解析，且无配对结果会使下轮请求结构非法）。
        状态从未填充（流未开始/非流式路径）返回 None。
        """
        state = accumulator.state if accumulator is not None else None
        if not isinstance(state, _StreamState):
            return None
        return self._ordered_finalized_blocks(state) or None

    def pending_tool_calls(
        self, accumulator: StreamAccumulator | None
    ) -> list[PendingToolView]:
        """未终结 tool 调用投影——数据源 `state.pending_tools`。

        与 `_ordered_finalized_blocks` 共用同一状态且判定互斥：凡 index 仍在
        `pending_tools` 中的块即未终结（snapshot 跳过它们），这里恰恰取出它们。
        `args_fragment` 搬运原始 args 文本累积（`args_buffer`），后端不解析。
        截断检测走 `unfinished_tool_calls()`（不做 id 过滤的计数口径）。
        """
        state = accumulator.state if accumulator is not None else None
        if not isinstance(state, _StreamState):
            return []
        views: list[PendingToolView] = []
        for idx in sorted(state.pending_tools):
            call = state.pending_tools[idx]
            if not call.id:
                continue  # 尚无 id 的半截调用无法锚定，跳过
            views.append(
                PendingToolView(
                    tool_call_id=call.id,
                    tool_name=call.name,
                    args_fragment=call.args_buffer,
                )
            )
        return views

    def unfinished_tool_calls(self, accumulator: StreamAccumulator | None) -> int:
        """未终结 tool call 计数——截断检测；含无 id 的半截调用（无盲区）。"""
        state = accumulator.state if accumulator is not None else None
        if not isinstance(state, _StreamState):
            return 0
        return len(state.pending_tools)

    async def list_models(self) -> list[str]:
        if self._config.models:
            # 静态声明短路：字符串 / 对象两种形态统一取实际调用名（排序保持现状）。
            return sorted(self._config.model_names())
        try:
            resp = await self._client.get("/v1/models")
            await raise_with_body(resp)
            data = resp.json()
            return sorted([m["id"] for m in data.get("data", [])])
        except Exception as e:
            log.error(f"Failed to list models: {e}")
            raise

    async def aclose(self) -> None:
        await self._client.aclose()

    @property
    def thinking(self) -> bool:
        """thinking 开关状态——从 extra_body 的 thinking 配置推导。

        Anthropic 的 thinking 由 extra_body（thinking.type=enabled）控制，
        该配置平铺进请求 body。状态从它推导，保证「序列化回放 / 对外
        get_status() 上报 / 开关语义」三者自洽。set_thinking() 直接改写
        这份配置（运行时开关与静态配置走同一存储）。
        """
        return self._thinking_enabled(self._extra_body)

    @staticmethod
    def _thinking_enabled(extra_body: dict) -> bool:
        """extra_body 是否启用 thinking（Anthropic 官方开关：thinking.type=enabled）。"""
        tb = extra_body.get("thinking")
        return isinstance(tb, dict) and tb.get("type") == "enabled"

    def set_thinking(self, enable: bool) -> None:
        """运行时 thinking 开关：改写 extra_body.thinking（请求 body 透传源）。

        与 OpenAI-compat 路径同构：状态取自实际 payload 源，property 派生 /
        get_status 上报 / 请求体三者自洽。启用时缺 budget_tokens 则补默认预算
        （Anthropic 要求 type=enabled 必带 budget）；关闭只改 type、保留
        budget——序列化时按 Anthropic 校验剥离（disabled 不得携带 budget），
        再启用时用户原预算原样恢复。
        """
        tb = self._extra_body.get("thinking")
        if not isinstance(tb, dict):
            tb = {}
            self._extra_body["thinking"] = tb
        if enable:
            tb["type"] = "enabled"
            tb.setdefault("budget_tokens", _DEFAULT_THINKING_BUDGET)
        else:
            tb["type"] = "disabled"

    def set_reasoning_effort(self, effort: str | None) -> None:
        # Anthropic 协议无 reasoning_effort 概念（百炼用 output_config.effort，走 extra_body）。
        # No-op：不持有状态，不影响请求。
        pass

    # ─── Request Building ─────────────────────────────────────────

    @staticmethod
    def _make_headers(api_key: str, anthropic_version: str) -> dict:
        """构造静态请求 header（client 创建时固化）。

        interleaved thinking beta header 随运行时 thinking 状态变化，
        由 _request_headers() 每请求计算，不在此固化。
        """
        return {
            "x-api-key": api_key,
            "anthropic-version": anthropic_version,
            "Content-Type": "application/json",
        }

    def _request_headers(self) -> dict:
        """每请求动态 header：thinking 启用时携带 interleaved thinking beta header
        （缺它则工具轮次间不会产生多块 thinking）。

        运行时 set_thinking() 改写 extra_body.thinking 后 header 必须跟随，
        故 MUST NOT 在 client 创建时固化。
        """
        if self._thinking_enabled(self._extra_body):
            return {"anthropic-beta": "interleaved-thinking-2025-05-14"}
        return {}

    def _build_body(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None,
        stream: bool,
    ) -> dict:
        system_text, anthropic_messages = self._serialize_messages(messages, model)

        body: dict = {
            "model": model,
            "messages": anthropic_messages,
            "max_tokens": self._config.max_tokens,
            "stream": stream,
        }
        if system_text:
            if self.explicit_cache_mode:
                body["system"] = [
                    {
                        "type": "text",
                        "text": system_text,
                        "cache_control": {"type": "ephemeral"},
                    }
                ]
            else:
                body["system"] = system_text
        if tools:
            body["tools"] = [self._tool_to_anthropic(t) for t in tools]

        # extra_body 透传（不覆盖已设置的 key）
        for k, v in self._extra_body.items():
            if k not in body:
                body[k] = v

        # Anthropic 校验：type=disabled 的 thinking 不得携带 budget_tokens。
        # 状态层（_extra_body）保留 budget（toggle 再启用时原样恢复），仅序列化
        # 时剥离——浅拷贝写 body，MUST NOT 改动 _extra_body。
        tb = body.get("thinking")
        if (
            isinstance(tb, dict)
            and tb.get("type") != "enabled"
            and "budget_tokens" in tb
        ):
            body["thinking"] = {k: v for k, v in tb.items() if k != "budget_tokens"}

        # 缓存标记
        if self.explicit_cache_mode and anthropic_messages:
            self._apply_cache_control(anthropic_messages)

        return body

    # ─── Non-streaming ────────────────────────────────────────────

    async def _generate_sync(
        self, body: dict, model: str
    ) -> AsyncIterator[LLMResponse]:
        t0 = time.monotonic()
        try:
            resp = await asyncio.wait_for(
                self._client.post(
                    "/v1/messages", json=body, headers=self._request_headers()
                ),
                timeout=self.timeout_total,
            )
            await raise_with_body(resp)
        except asyncio.TimeoutError:
            log.error(f"LLM request timeout after {self.timeout_total}s")
            raise TimeoutError(f"LLM request timeout after {self.timeout_total}s")

        request_id = resp.headers.get("request-id", "")
        data = resp.json()
        elapsed = time.monotonic() - t0
        log.info("[DONE] anthropic sync call")

        raw_blocks = data.get("content", [])
        blocks: list[TextBlock | ThinkingBlock | ToolUseBlock] = []
        text_parts: list[str] = []
        reasoning_parts: list[str] = []
        tool_calls: list[ToolCall] = []

        for block in raw_blocks:
            btype = block.get("type")
            if btype == "text":
                text = block.get("text", "")
                text_parts.append(text)
                blocks.append(TextBlock(text=text))
            elif btype == "thinking":
                thinking = block.get("thinking", "")
                reasoning_parts.append(thinking)
                blocks.append(
                    ThinkingBlock(thinking=thinking, signature=block.get("signature"))
                )
            elif btype == "redacted_thinking":
                # 加密 payload 当不透明黑盒，存进 signature，原样回放永不解析
                blocks.append(
                    ThinkingBlock(
                        thinking="", signature=block.get("data", ""), redacted=True
                    )
                )
            elif btype == "tool_use":
                tool_calls.append(
                    ToolCall(
                        id=block["id"],
                        name=block["name"],
                        arguments=block.get("input", {}),
                    )
                )
                blocks.append(
                    ToolUseBlock(
                        id=block["id"], name=block["name"], input=block.get("input", {})
                    )
                )

        # 零信息块剔除（与流式路径 / OpenAI 路径对齐）
        blocks = [b for b in blocks if not _is_zero_info_block(b)]

        usage_data = data.get("usage", {})
        log.debug(f"[anthropic usage] sync raw: {usage_data}")
        prompt_tokens = usage_data.get("input_tokens", 0)
        completion_tokens = usage_data.get("output_tokens", 0)
        cached_tokens = usage_data.get("cache_read_input_tokens", 0)
        cache_creation = usage_data.get("cache_creation_input_tokens", 0)
        # 与流式路径/OpenAI 路径口径一致：prompt_tokens = 总输入（含缓存）
        prompt_tokens = prompt_tokens + cached_tokens + cache_creation

        yield LLMResponse(
            content="".join(text_parts) or None,
            reasoning_content="".join(reasoning_parts) or None,
            # 空响应 → 空块数组（合法协议输出，与 OpenAI 路径统一；
            # 轮有效性由 ReActLoop 判定——空数组会被判无效并重试）
            content_blocks=blocks,
            tool_calls=tool_calls or None,
            usage=LLMUsage(
                prompt_tokens=prompt_tokens,
                completion_tokens=completion_tokens,
                cached_tokens=cached_tokens,
                first_chunk_rt_ms=elapsed * 1000,
                tokens_per_sec=completion_tokens / elapsed if elapsed > 0 else 0.0,
                model=model,
                request_id=request_id,
                stop_reason=data.get("stop_reason"),
            ),
        )

    # ─── Streaming ────────────────────────────────────────────────

    async def _generate_stream(
        self, body: dict, model: str, accumulator: StreamAccumulator | None = None
    ) -> AsyncIterator[LLMResponse]:
        t0 = time.monotonic()
        try:
            req = self._client.build_request(
                "POST", "/v1/messages", json=body, headers=self._request_headers()
            )
            resp = await asyncio.wait_for(
                self._client.send(req, stream=True),
                timeout=self.timeout_first_chunk,
            )
            await raise_with_body(resp)
        except asyncio.TimeoutError:
            # 只包住 send(stream=True)：等价于"响应头超时"，不是首 token 超时
            # ——响应体自身的停滞由 lines_with_idle_timeout 判定。
            log.error(
                f"LLM response header timeout after {self.timeout_first_chunk}s "
                f"(no response to the streaming request)"
            )
            raise TimeoutError(
                f"LLM response header timeout after {self.timeout_first_chunk}s "
                f"(no response to the streaming request)"
            )

        # 累积状态装入 caller 持有的容器（每次尝试重置——重试重传时
        # 快照只反映当前尝试，与消费者看到的内容一致）
        state = _StreamState()
        if accumulator is not None:
            accumulator.state = state

        request_id = resp.headers.get("request-id", "")
        first_chunk_rt_ms = (time.monotonic() - t0) * 1000
        log.info("[DONE] anthropic response header received")

        try:
            async for event in parse_sse_stream(
                lines_with_idle_timeout(
                    resp.aiter_lines(),
                    timeout=STREAM_IDLE_TIMEOUT,
                    context=f"anthropic {model}",
                )
            ):
                data = parse_json_event(event)
                if data is None:
                    continue
                event_type = data.get("type", "")

                if event_type == "content_block_start":
                    self._on_block_start(state, data)
                elif event_type == "content_block_delta":
                    out = self._on_block_delta(
                        state, data, model, request_id, first_chunk_rt_ms
                    )
                    if out is not None:
                        yield out
                elif event_type == "content_block_stop":
                    out = self._on_block_stop(
                        state, data, model, request_id, first_chunk_rt_ms
                    )
                    if out is not None:
                        yield out
                elif event_type == "message_delta":
                    self._on_message_delta(state, data)
                elif event_type == "message_start":
                    self._on_message_start(state, data)
                elif event_type == "message_stop":
                    yield self._build_final_response(
                        state, model, request_id, first_chunk_rt_ms
                    )
                elif event_type == "error":
                    # Anthropic 流中 error 事件（overloaded / rate limit / 中途
                    # 终止）：抛出触发 with_retry 重试与错误上报——截断轮次绝不
                    # 作为成功 turn 提交。
                    raise ProviderStreamError(data)
        finally:
            await resp.aclose()
