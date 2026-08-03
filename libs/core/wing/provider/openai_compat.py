# wing/provider/openai_compat.py
"""OpenAI 兼容协议 Provider — 基于 httpx 实现。"""

from __future__ import annotations

import asyncio
import json
import time
from typing import TYPE_CHECKING, AsyncIterator

import httpx

from wing.common.logger import log
from wing.common.with_retry import with_retry
from wing.config import get_headers
from wing.provider.base import ModelProvider
from wing.provider.errors import raise_with_body
from wing.provider.http import make_http_timeout
from wing.provider.sse import parse_json_event, parse_sse_stream
from wing.schema import (
    ContentBlock,
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


class OpenAICompatProvider(ModelProvider):
    """OpenAI 兼容协议 provider（httpx 实现）。"""

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
        # 基线默认行为（对齐 develop）：enable_thinking / preserve_thinking 默认
        # 随每个请求发送（用户 extra_body 的显式值优先）。preserve_thinking 尤为
        # 关键——缺它则多轮工具回合间 thinking 被服务端剥离。
        # thinking 状态从 extra_body 派生（实际请求 payload 源），与 Anthropic
        # 路径同构：property / setter / 请求体 / 对外上报四者自洽。
        self._extra_body.setdefault("enable_thinking", True)
        self._extra_body.setdefault("preserve_thinking", True)

        headers = get_headers()
        headers["Authorization"] = f"Bearer {config.api_key}"
        headers["Content-Type"] = "application/json"

        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            headers=headers,
            timeout=make_http_timeout(),
        )
        log.info(f"OpenAICompatProvider initialized: {self.base_url}")

    # ─── Public API ───────────────────────────────────────────────

    @with_retry()
    async def generate(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        log.info(
            f"[BEGIN] openai_compat call {model} with {len(messages)} stream:{stream}"
        )
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
            resp = await self._client.get("/models")
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
        """thinking 开关状态——从 extra_body 的 enable_thinking 派生。

        extra_body 平铺进请求 body，是实际 payload 源；状态从它派生，保证
        「序列化回放 / 对外 get_status() 上报 / 开关语义」三者自洽
        （与 Anthropic 路径同构）。
        """
        return bool(self._extra_body.get("enable_thinking", True))

    def set_thinking(self, enable: bool) -> None:
        # 改写 extra_body（请求 body 透传源）：切换下次请求即生效，
        # property 派生随之翻转，无第二份状态。
        self._extra_body["enable_thinking"] = enable

    def set_reasoning_effort(self, effort: str | None) -> None:
        self.reasoning_effort = effort

    # ─── Request Building ─────────────────────────────────────────

    @staticmethod
    def _build_content_blocks(
        reasoning: str | None,
        content: str | None,
        tool_calls: list[ToolCall],
    ) -> list[ContentBlock]:
        """从 OpenAI 扁平响应构建权威块数组（与 Anthropic 路径输出契约统一）。

        OpenAI 扁平协议无块序信息，约定序为 thinking → text → tool_use；
        reasoning 映射为无签名 ThinkingBlock（OpenAI 协议无签名概念）。
        """
        blocks: list[ContentBlock] = []
        if reasoning:
            blocks.append(ThinkingBlock(thinking=reasoning))
        if content:
            blocks.append(TextBlock(text=content))
        for tc in tool_calls:
            blocks.append(ToolUseBlock(id=tc.id, name=tc.name, input=tc.arguments))
        return blocks

    def _build_body(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None,
        stream: bool,
    ) -> dict:
        openai_messages = [m.to_openai() for m in messages]
        if self.explicit_cache_mode:
            self._apply_cache_control(openai_messages)

        body: dict = {
            "model": model,
            "messages": openai_messages,
            "stream": stream,
        }
        if tools:
            body["tools"] = [t.to_openai() for t in tools]
        if stream:
            body["stream_options"] = {"include_usage": True}

        # extra_body 透传（不覆盖已设置的 key）
        for k, v in self._extra_body.items():
            if k not in body:
                body[k] = v

        # reasoning_effort
        if self.reasoning_effort:
            body["reasoning_effort"] = self.reasoning_effort

        # prompt_cache_key（显式缓存模式）
        if self.explicit_cache_mode and self._session_id:
            body["prompt_cache_key"] = self._session_id

        return body

    # ─── Non-streaming ────────────────────────────────────────────

    async def _generate_sync(
        self, body: dict, model: str
    ) -> AsyncIterator[LLMResponse]:
        t0 = time.monotonic()
        try:
            resp = await asyncio.wait_for(
                self._client.post("/chat/completions", json=body),
                timeout=self.timeout_total,
            )
            await raise_with_body(resp)
        except asyncio.TimeoutError:
            log.error(f"LLM request timeout after {self.timeout_total}s")
            raise TimeoutError(f"LLM request timeout after {self.timeout_total}s")

        request_id = resp.headers.get("x-request-id", "")
        data = resp.json()
        elapsed = time.monotonic() - t0
        log.info("[DONE] openai_compat sync call")

        choice = data["choices"][0]
        message = choice["message"]
        tool_calls = [
            ToolCall(
                id=tc["id"],
                name=tc["function"]["name"],
                arguments=json.loads(tc["function"]["arguments"]),
            )
            for tc in (message.get("tool_calls") or [])
        ]

        usage_data = data.get("usage") or {}
        prompt_tokens = usage_data.get("prompt_tokens", 0)
        completion_tokens = usage_data.get("completion_tokens", 0)
        cached_tokens = (usage_data.get("prompt_tokens_details", {}) or {}).get(
            "cached_tokens", 0
        ) or 0

        yield LLMResponse(
            content=message.get("content"),
            reasoning_content=message.get("reasoning_content"),
            tool_calls=tool_calls or None,
            content_blocks=self._build_content_blocks(
                message.get("reasoning_content"), message.get("content"), tool_calls
            ),
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
            req = self._client.build_request("POST", "/chat/completions", json=body)
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

        request_id = resp.headers.get("x-request-id", "")
        first_chunk_rt_ms = (time.monotonic() - t0) * 1000
        log.info("[DONE] openai_compat stream call connected")

        pending: dict[int, PendingCall] = {}
        first_token_ts: float | None = None
        # 块数组累积：流结束时产出权威 content_blocks（与 Anthropic 路径契约统一）
        reasoning_chunks: list[str] = []
        content_chunks: list[str] = []
        final_tool_calls: list[ToolCall] = []

        try:
            async for event in parse_sse_stream(resp.aiter_lines()):
                chunk = parse_json_event(event)
                if chunk is None:
                    continue

                choices = chunk.get("choices") or []
                choice = choices[0] if choices else None
                delta = choice.get("delta", {}) if choice else {}

                # Usage（通常在最后一个 chunk）
                usage_data = chunk.get("usage") or {}
                prompt_tokens = usage_data.get("prompt_tokens", 0)
                completion_tokens = usage_data.get("completion_tokens", 0)
                cached_tokens = (usage_data.get("prompt_tokens_details", {}) or {}).get(
                    "cached_tokens", 0
                ) or 0

                reasoning = delta.get("reasoning_content")
                content = delta.get("content")

                if reasoning:
                    reasoning_chunks.append(reasoning)
                if content:
                    content_chunks.append(content)

                if first_token_ts is None and (content or reasoning):
                    first_token_ts = time.monotonic()

                tcs, deltas = (
                    self._process_tool_deltas(choice, pending)
                    if choice
                    else (None, None)
                )
                if tcs:
                    final_tool_calls.extend(tcs)

                decode_tps = 0.0
                if completion_tokens > 0 and first_token_ts is not None:
                    decode_elapsed = time.monotonic() - first_token_ts
                    if decode_elapsed > 0:
                        decode_tps = completion_tokens / decode_elapsed

                chunk_usage = LLMUsage(
                    prompt_tokens=prompt_tokens,
                    completion_tokens=completion_tokens,
                    cached_tokens=cached_tokens,
                    first_chunk_rt_ms=first_chunk_rt_ms,
                    tokens_per_sec=decode_tps,
                    model=model,
                    request_id=request_id,
                )

                yield LLMResponse(
                    content=content,
                    reasoning_content=reasoning,
                    tool_calls=tcs,
                    tool_call_deltas=deltas,
                    usage=chunk_usage,
                )

            # 流结束：产出权威 content_blocks（ReActLoop 以此为 Message 唯一组装
            # 依据）。usage 只带零 token 元信息——非零 usage 已由带内 usage chunk
            # 触发过上游 metrics 发射，此处再附着会造成 token 双计；流无带内
            # usage 时 Message.usage 仍保留 first_chunk_rt_ms 等元信息。
            yield LLMResponse(
                content_blocks=self._build_content_blocks(
                    "".join(reasoning_chunks) or None,
                    "".join(content_chunks) or None,
                    final_tool_calls,
                ),
                usage=LLMUsage(
                    first_chunk_rt_ms=first_chunk_rt_ms,
                    model=model,
                    request_id=request_id,
                ),
            )
        finally:
            await resp.aclose()

    # ─── Tool Delta Processing ────────────────────────────────────

    @staticmethod
    def _process_tool_deltas(
        choice: dict,
        pending: dict[int, PendingCall],
    ) -> tuple[list[ToolCall] | None, list[ToolCallDelta] | None]:
        """Process tool call deltas from a streaming chunk dict.

        Returns:
            (final_tool_calls, streaming_deltas)
        """
        delta = choice.get("delta", {})
        tc_deltas = delta.get("tool_calls")
        has_tool_delta = bool(tc_deltas)

        for tc in tc_deltas or []:
            idx = tc.get("index", 0)
            call = pending.setdefault(idx, PendingCall())
            if tc.get("id"):
                call.id = tc["id"]
            func = tc.get("function") or {}
            if func.get("name"):
                call.name = func["name"]
            if func.get("arguments"):
                call.args_buffer += func["arguments"]

        deltas: list[ToolCallDelta] | None = None
        if has_tool_delta and pending:
            is_final = choice.get("finish_reason") == "tool_calls"
            deltas = []
            for call in pending.values():
                fragment = call.args_buffer[call.emitted_len :]
                if not call.id or not fragment:
                    continue
                call.emitted_len = len(call.args_buffer)
                deltas.append(
                    ToolCallDelta(
                        id=call.id,
                        name=call.name,
                        args_fragment=fragment,
                        is_final=is_final,
                    )
                )
            if not deltas:
                deltas = None

        finals: list[ToolCall] | None = None
        if choice.get("finish_reason") == "tool_calls":
            finals = [
                ToolCall(
                    id=call.id,
                    name=call.name,
                    arguments=json.loads(call.args_buffer),
                )
                for call in pending.values()
            ]
            pending.clear()

        return finals, deltas

    # ─── Cache Control ────────────────────────────────────────────

    @staticmethod
    def _apply_cache_control(openai_messages: list[dict]) -> None:
        """为最后一条消息的最后一个 content block 追加 cache_control 标记。"""
        if not openai_messages:
            return
        last_msg = openai_messages[-1]
        content = last_msg.get("content")
        if content is None:
            return
        if isinstance(content, str):
            last_msg["content"] = [
                {
                    "type": "text",
                    "text": content,
                    "cache_control": {"type": "ephemeral"},
                }
            ]
        elif isinstance(content, list):
            last_msg["content"][-1]["cache_control"] = {"type": "ephemeral"}
