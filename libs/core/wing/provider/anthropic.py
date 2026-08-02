# wing/provider/anthropic.py
"""Anthropic 协议 Provider — 基于 httpx 裸写。

处理 Anthropic Messages API 的结构差异：
- system prompt 为顶层字段
- content 为 block 数组（text / tool_use / tool_result / thinking）
- 流式事件为 content_block_start/delta/stop + message_delta
- 认证用 x-api-key header（非 Bearer）
"""

from __future__ import annotations

import asyncio
import json
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, AsyncIterator

import httpx

from wing.common.logger import log
from wing.common.with_retry import with_retry
from wing.provider.base import ModelProvider
from wing.provider.errors import ProviderStreamError, raise_with_body
from wing.provider.http import make_http_timeout
from wing.provider.sse import parse_json_event, parse_sse_stream
from wing.schema import (
    LLMResponse,
    LLMUsage,
    Message,
    PendingCall,
    TextBlock,
    ThinkingBlock,
    Tool,
    ToolCall,
    ToolCallDelta,
    ToolUseBlock,
)

if TYPE_CHECKING:
    from wing.config import ProviderConfig


@dataclass
class _StreamState:
    """Anthropic 流式解析状态——per-index 结构化。

    块与 tool 参数均按事件 index 独立累积，不依赖"块事件严格顺序"的隐式
    假设：交错到达的 delta 各归其 index，content_block_stop 按其 index
    终结对应块。
    """

    blocks_by_index: dict[int, TextBlock | ThinkingBlock | ToolUseBlock] = field(
        default_factory=dict
    )
    pending_tools: dict[int, PendingCall] = field(default_factory=dict)
    # usage 累计
    prompt_tokens: int = 0
    completion_tokens: int = 0
    cached_tokens: int = 0
    cache_creation: int = 0
    input_source: str = "start"  # input_tokens 来源：delta（权威）或 start（兜底）
    first_token_ts: float | None = None


class AnthropicProvider(ModelProvider):
    """Anthropic Messages API provider（httpx 实现）。"""

    def __init__(
        self,
        config: ProviderConfig,
        session_id: str | None = None,
    ) -> None:
        self._config = config
        self._session_id = session_id
        self.base_url = config.base_url.rstrip("/")
        self.reasoning_effort: str | None = config.reasoning_effort
        self.timeout_first_chunk = config.timeout_first_chunk
        self.timeout_total = config.timeout_total
        self.explicit_cache_mode = config.explicit_cache_mode
        self._extra_body: dict = dict(config.extra_body)
        self._anthropic_version = config.anthropic_version

        headers = self._make_headers(
            config.api_key, self._anthropic_version, self._extra_body
        )

        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            headers=headers,
            timeout=make_http_timeout(),
        )
        log.info(f"AnthropicProvider initialized: {self.base_url}")

    # ─── Public API ───────────────────────────────────────────────

    @with_retry(max_retries=2)
    async def generate(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        log.info(f"[BEGIN] anthropic call {model} with {len(messages)} stream:{stream}")
        body = self._build_body(messages, model, tools, stream)

        if stream:
            async for item in self._generate_stream(body, model):
                yield item
        else:
            async for item in self._generate_sync(body, model):
                yield item

    async def list_models(self) -> list[str]:
        if self._config.models:
            return sorted(self._config.models)
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
        get_status() 上报 / 开关语义」三者自洽。
        """
        return self._thinking_enabled(self._extra_body)

    @staticmethod
    def _thinking_enabled(extra_body: dict) -> bool:
        """extra_body 是否启用 thinking（Anthropic 官方开关：thinking.type=enabled）。"""
        tb = extra_body.get("thinking")
        return isinstance(tb, dict) and tb.get("type") == "enabled"

    def set_thinking(self, enable: bool) -> None:
        # Anthropic 协议不通过此 flag 控制 thinking；用户应通过 extra_body 配置。
        # No-op：不持有状态，不影响请求。
        pass

    def set_reasoning_effort(self, effort: str | None) -> None:
        # Anthropic 协议无 reasoning_effort 概念（百炼用 output_config.effort，走 extra_body）。
        # No-op：不持有状态，不影响请求。
        pass

    # ─── Request Building ─────────────────────────────────────────

    @staticmethod
    def _make_headers(api_key: str, anthropic_version: str, extra_body: dict) -> dict:
        """构造请求 header。启用 thinking 时携带交错 thinking beta header
        （缺它则工具轮次间不会产生多块 thinking）。"""
        headers = {
            "x-api-key": api_key,
            "anthropic-version": anthropic_version,
            "Content-Type": "application/json",
        }
        if AnthropicProvider._thinking_enabled(extra_body):
            headers["anthropic-beta"] = "interleaved-thinking-2025-05-14"
        return headers

    def _build_body(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None,
        stream: bool,
    ) -> dict:
        system_text, anthropic_messages = self._serialize_messages(messages)

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

        # 缓存标记
        if self.explicit_cache_mode and anthropic_messages:
            self._apply_cache_control(anthropic_messages)

        return body

    def _serialize_messages(self, messages: list[Message]) -> tuple[str, list[dict]]:
        """将 Message 列表转换为 Anthropic 格式。

        Returns:
            (system_text, anthropic_messages)
        """
        system_parts: list[str] = []
        anthropic_msgs: list[dict] = []

        for msg in messages:
            if msg.role == "system":
                if msg.content:
                    system_parts.append(msg.content)
                continue

            if msg.role == "assistant":
                blocks = self._serialize_assistant(msg)
                # 零块 assistant（如纯 thinking 轮的 thinking 块被 clear_reasoning
                # 剥离后）MUST NOT 发出 content: []——Anthropic 要求 content 至少
                # 一个块，否则本次及该 session 后续所有请求 400。丢弃是配对安全
                # 的：零块即无 tool_use，不会有后续 tool_result 引用本条。
                if not blocks:
                    continue
                anthropic_msgs.append({"role": "assistant", "content": blocks})

            elif msg.role == "tool":
                # tool result → user 消息中的 tool_result block
                anthropic_msgs.append(
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": msg.tool_call_id or "",
                                "content": msg.content or "",
                            }
                        ],
                    }
                )

            elif msg.role == "user":
                if msg.content:
                    anthropic_msgs.append(
                        {
                            "role": "user",
                            "content": [{"type": "text", "text": msg.content}],
                        }
                    )

        # 合并连续的 user 消息：Anthropic Messages API 要求 user/assistant
        # 严格交替，连续同角色返回 400。wing 的真实产生路径：tool 消息序列化为
        # user（tool_result）后，紧跟 steer 注入的 user 消息。
        merged = self._merge_consecutive_user(anthropic_msgs)
        return "\n\n".join(system_parts), merged

    def _serialize_assistant(self, msg: Message) -> list[dict]:
        """将 assistant 消息的 content_blocks 按序一对一映射回 Anthropic block。

        忠实回放契约：
        - thinking 有 signature → 原样按序发出，一个字节都不改
          （改了会签名失配 + 击穿 prompt cache）
        - thinking 无 signature → 原样发空签名（signature:""）。此类数据
          产生于不下发签名的推理 provider 或存量旧会话——回放官方 Anthropic
          被拒是预期行为（官方必下发签名），MUST NOT 降级 text 破坏内容语义
        - thinking 文本为空且无 signature → 整块丢弃
        - redacted → redacted_thinking + data（不透明黑盒原样回放）
        """
        blocks: list[dict] = []
        for block in msg.content_blocks or []:
            if isinstance(block, TextBlock):
                if block.text:
                    blocks.append({"type": "text", "text": block.text})
            elif isinstance(block, ThinkingBlock):
                if block.redacted:
                    blocks.append(
                        {"type": "redacted_thinking", "data": block.signature or ""}
                    )
                elif not block.thinking.strip() and not block.signature:
                    continue  # 空 thinking 无签名 → 丢弃
                else:
                    blocks.append(
                        {
                            "type": "thinking",
                            "thinking": block.thinking,
                            "signature": block.signature or "",
                        }
                    )
            elif isinstance(block, ToolUseBlock):
                blocks.append(
                    {
                        "type": "tool_use",
                        "id": block.id,
                        "name": block.name,
                        "input": block.input,
                    }
                )
        return blocks

    @staticmethod
    def _merge_consecutive_user(msgs: list[dict]) -> list[dict]:
        """合并连续的 user 消息（tool_result 后紧跟 user 文本的情况）。"""
        if not msgs:
            return msgs
        merged: list[dict] = [msgs[0]]
        for msg in msgs[1:]:
            prev = merged[-1]
            if msg["role"] == "user" and prev["role"] == "user":
                # 合并 content blocks
                prev["content"].extend(msg["content"])
            else:
                merged.append(msg)
        return merged

    @staticmethod
    def _tool_to_anthropic(tool: Tool) -> dict:
        """将 Tool 转换为 Anthropic tool 定义格式。"""
        openai_def = tool.to_openai()
        func = openai_def["function"]
        return {
            "name": func["name"],
            "description": func.get("description", ""),
            "input_schema": func.get(
                "parameters", {"type": "object", "properties": {}}
            ),
        }

    @staticmethod
    def _apply_cache_control(anthropic_messages: list[dict]) -> None:
        """为最后一条消息的最后一个 content block 打 cache_control。

        与 OpenAI 路径同构：cache_control 是附加字段，不改写 thinking 字节、
        不影响签名；Anthropic prompt caching 支持 thinking 块携带缓存标记。
        """
        if not anthropic_messages:
            return
        last_msg = anthropic_messages[-1]
        content = last_msg.get("content")
        if not isinstance(content, list) or not content:
            return
        content[-1]["cache_control"] = {"type": "ephemeral"}

    # ─── Non-streaming ────────────────────────────────────────────

    async def _generate_sync(
        self, body: dict, model: str
    ) -> AsyncIterator[LLMResponse]:
        t0 = time.monotonic()
        try:
            resp = await asyncio.wait_for(
                self._client.post("/v1/messages", json=body),
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
            content_blocks=blocks or None,
            tool_calls=tool_calls or None,
            usage=LLMUsage(
                prompt_tokens=prompt_tokens,
                completion_tokens=completion_tokens,
                cached_tokens=cached_tokens,
                first_chunk_rt_ms=elapsed * 1000,
                tokens_per_sec=completion_tokens / elapsed if elapsed > 0 else 0.0,
                model=model,
                request_id=request_id,
            ),
        )

    # ─── Streaming ────────────────────────────────────────────────

    async def _generate_stream(
        self, body: dict, model: str
    ) -> AsyncIterator[LLMResponse]:
        t0 = time.monotonic()
        try:
            req = self._client.build_request("POST", "/v1/messages", json=body)
            resp = await asyncio.wait_for(
                self._client.send(req, stream=True),
                timeout=self.timeout_first_chunk,
            )
            await raise_with_body(resp)
        except asyncio.TimeoutError:
            log.error(f"LLM first chunk timeout after {self.timeout_first_chunk}s")
            raise TimeoutError(
                f"LLM first chunk timeout after {self.timeout_first_chunk}s"
            )

        request_id = resp.headers.get("request-id", "")
        first_chunk_rt_ms = (time.monotonic() - t0) * 1000
        log.info("[DONE] anthropic stream connected")

        state = _StreamState()
        try:
            async for event in parse_sse_stream(resp.aiter_lines()):
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

    # ─── Stream Event Handlers ────────────────────────────────────

    @staticmethod
    def _on_block_start(state: _StreamState, data: dict) -> None:
        idx = data.get("index", 0)
        block = data.get("content_block", {})
        btype = block.get("type", "")
        if btype == "thinking":
            state.blocks_by_index[idx] = ThinkingBlock(thinking="", signature="")
        elif btype == "redacted_thinking":
            # 加密 payload 当不透明黑盒，存进 signature，redacted=True
            state.blocks_by_index[idx] = ThinkingBlock(
                thinking="", signature=block.get("data", ""), redacted=True
            )
        elif btype == "text":
            state.blocks_by_index[idx] = TextBlock(text="")
        elif btype == "tool_use":
            tool_id = block.get("id", "")
            tool_name = block.get("name", "")
            state.blocks_by_index[idx] = ToolUseBlock(
                id=tool_id, name=tool_name, input={}
            )
            state.pending_tools[idx] = PendingCall(id=tool_id, name=tool_name)

    @staticmethod
    def _on_block_delta(
        state: _StreamState,
        data: dict,
        model: str,
        request_id: str,
        first_chunk_rt_ms: float,
    ) -> LLMResponse | None:
        """处理 content_block_delta：累积进对应 index 的块，发射增量事件。"""
        idx = data.get("index", 0)
        delta = data.get("delta", {})
        delta_type = delta.get("type", "")
        meta = LLMUsage(
            first_chunk_rt_ms=first_chunk_rt_ms, model=model, request_id=request_id
        )

        if delta_type == "text_delta":
            text = delta.get("text", "")
            if state.first_token_ts is None and text:
                state.first_token_ts = time.monotonic()
            blk = state.blocks_by_index.get(idx)
            if isinstance(blk, TextBlock):
                blk.text += text
            return LLMResponse(content=text, usage=meta)

        if delta_type == "thinking_delta":
            thinking = delta.get("thinking", "")
            if state.first_token_ts is None and thinking:
                state.first_token_ts = time.monotonic()
            blk = state.blocks_by_index.get(idx)
            if isinstance(blk, ThinkingBlock):
                blk.thinking += thinking
            return LLMResponse(reasoning_content=thinking, usage=meta)

        if delta_type == "signature_delta":
            # per-block signature：累加进对应块（兼容单片/多片）
            sig = delta.get("signature", "")
            blk = state.blocks_by_index.get(idx)
            if isinstance(blk, ThinkingBlock) and sig:
                blk.signature = (blk.signature or "") + sig
            return None

        if delta_type == "input_json_delta":
            call = state.pending_tools.get(idx)
            if call is None:
                return None
            call.args_buffer += delta.get("partial_json", "")
            fragment = call.args_buffer[call.emitted_len :]
            if not fragment or not call.id:
                return None
            call.emitted_len = len(call.args_buffer)
            return LLMResponse(
                tool_call_deltas=[
                    ToolCallDelta(
                        id=call.id,
                        name=call.name,
                        args_fragment=fragment,
                        is_final=False,
                    )
                ],
                usage=meta,
            )

        return None

    @staticmethod
    def _on_block_stop(
        state: _StreamState,
        data: dict,
        model: str,
        request_id: str,
        first_chunk_rt_ms: float,
    ) -> LLMResponse | None:
        """content_block_stop：按其 index 终结 tool 块（权威 JSON 解析）。"""
        idx = data.get("index", 0)
        call = state.pending_tools.pop(idx, None)
        if call is None or not call.id:
            return None
        try:
            args = json.loads(call.args_buffer) if call.args_buffer else {}
        except json.JSONDecodeError:
            log.warning(
                f"Failed to parse tool args JSON for "
                f"{call.name}: {call.args_buffer[:200]}"
            )
            args = {}
        blk = state.blocks_by_index.get(idx)
        if isinstance(blk, ToolUseBlock):
            blk.input = args
        return LLMResponse(
            tool_calls=[ToolCall(id=call.id, name=call.name, arguments=args)],
            tool_call_deltas=[
                ToolCallDelta(
                    id=call.id,
                    name=call.name,
                    args_fragment=call.args_buffer[call.emitted_len :],
                    is_final=True,
                )
            ],
            usage=LLMUsage(
                first_chunk_rt_ms=first_chunk_rt_ms, model=model, request_id=request_id
            ),
        )

    @staticmethod
    def _on_message_delta(state: _StreamState, data: dict) -> None:
        # 权威累计 usage（官方文档：message_delta 的 usage 为 cumulative，
        # 且当前版本 API 含 input_tokens；message_start 仅作兜底）。
        usage_delta = data.get("usage", {})
        log.debug(f"[anthropic usage] message_delta raw: {usage_delta}")
        if "input_tokens" in usage_delta:
            state.prompt_tokens = usage_delta["input_tokens"]
            state.input_source = "delta"
        if "output_tokens" in usage_delta:
            state.completion_tokens = usage_delta["output_tokens"]
        if "cache_read_input_tokens" in usage_delta:
            state.cached_tokens = usage_delta["cache_read_input_tokens"]
        if "cache_creation_input_tokens" in usage_delta:
            state.cache_creation = usage_delta["cache_creation_input_tokens"]

    @staticmethod
    def _on_message_start(state: _StreamState, data: dict) -> None:
        # 兜底：标准 Anthropic 把完整 usage（含 cache_creation/cache_read）
        # 放在 message_start，message_delta 仅含 output_tokens；部分代理则
        # 相反（见 message_delta 分支）。两处都读，谁带就用谁，delta 后到覆盖。
        usage_start = data.get("message", {}).get("usage", {})
        log.debug(f"[anthropic usage] message_start raw: {usage_start}")
        if "input_tokens" in usage_start:
            state.prompt_tokens = usage_start["input_tokens"]
            state.input_source = "start"
        if "cache_read_input_tokens" in usage_start:
            state.cached_tokens = usage_start["cache_read_input_tokens"]
        if "cache_creation_input_tokens" in usage_start:
            state.cache_creation = usage_start["cache_creation_input_tokens"]

    @staticmethod
    def _build_final_response(
        state: _StreamState, model: str, request_id: str, first_chunk_rt_ms: float
    ) -> LLMResponse:
        """message_stop：产出有序 content_blocks（权威块数组）+ 最终 usage。"""
        # Anthropic 的 input_tokens 仅为非缓存部分（含兜底/增量源）；
        # 对齐 OpenAI 语义：prompt_tokens = 总输入（含缓存）
        total_prompt = state.prompt_tokens + state.cached_tokens + state.cache_creation
        hit_rate = state.cached_tokens / total_prompt * 100 if total_prompt else 0.0
        log.debug(
            f"[anthropic usage] final: "
            f"input(non-cached)={state.prompt_tokens}({state.input_source}) "
            f"cache_read={state.cached_tokens} "
            f"cache_creation={state.cache_creation} "
            f"output={state.completion_tokens} => "
            f"prompt_tokens(total)={total_prompt} "
            f"hit_rate={hit_rate:.1f}%"
        )
        decode_tps = 0.0
        if state.completion_tokens > 0 and state.first_token_ts is not None:
            decode_elapsed = time.monotonic() - state.first_token_ts
            if decode_elapsed > 0:
                decode_tps = state.completion_tokens / decode_elapsed
        ordered_blocks = [
            state.blocks_by_index[i] for i in sorted(state.blocks_by_index)
        ]
        return LLMResponse(
            content_blocks=ordered_blocks or None,
            usage=LLMUsage(
                prompt_tokens=total_prompt,
                completion_tokens=state.completion_tokens,
                cached_tokens=state.cached_tokens,
                first_chunk_rt_ms=first_chunk_rt_ms,
                tokens_per_sec=decode_tps,
                model=model,
                request_id=request_id,
            ),
        )
