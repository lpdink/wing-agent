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
from wing.provider.sse import parse_json_event, parse_sse_stream
from wing.schema import (
    LLMResponse,
    LLMUsage,
    Message,
    PendingCall,
    Tool,
    ToolCall,
    ToolCallDelta,
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
        self.thinking = True
        self.reasoning_effort: str | None = config.reasoning_effort
        self.timeout_first_chunk = config.timeout_first_chunk
        self.timeout_total = config.timeout_total
        self.explicit_cache_mode = config.explicit_cache_mode
        self._extra_body: dict = dict(config.extra_body)

        headers = get_headers()
        headers["Authorization"] = f"Bearer {config.api_key}"
        headers["Content-Type"] = "application/json"

        self._client = httpx.AsyncClient(
            base_url=self.base_url,
            headers=headers,
            timeout=httpx.Timeout(
                connect=30.0,
                read=self.timeout_first_chunk,
                write=30.0,
                pool=30.0,
            ),
        )
        log.info(f"OpenAICompatProvider initialized: {self.base_url}")

    # ─── Public API ───────────────────────────────────────────────

    @with_retry(max_retries=2)
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
            resp.raise_for_status()
            data = resp.json()
            return sorted([m["id"] for m in data.get("data", [])])
        except Exception as e:
            log.error(f"Failed to list models: {e}")
            raise

    async def aclose(self) -> None:
        await self._client.aclose()

    def set_thinking(self, enable: bool) -> None:
        self.thinking = enable

    def set_reasoning_effort(self, effort: str | None) -> None:
        self.reasoning_effort = effort

    def reload(self) -> list[str]:
        """从最新 config 刷新 provider 配置。返回变更项列表。"""
        from wing.config import get_config

        config = get_config()
        # 找到同名 provider 配置
        new_cfg = None
        for p in config.providers:
            if p.name == self._config.name:
                new_cfg = p
                break
        if new_cfg is None:
            return []

        changes: list[str] = []

        if new_cfg.base_url != self._config.base_url:
            self._config = new_cfg
            self.base_url = new_cfg.base_url.rstrip("/")
            headers = get_headers()
            headers["Authorization"] = f"Bearer {new_cfg.api_key}"
            headers["Content-Type"] = "application/json"
            old_client = self._client
            self._client = httpx.AsyncClient(
                base_url=self.base_url,
                headers=headers,
                timeout=httpx.Timeout(
                    connect=30.0,
                    read=new_cfg.timeout_first_chunk,
                    write=30.0,
                    pool=30.0,
                ),
            )
            # 异步关闭旧 client（fire-and-forget，不阻塞 reload）
            asyncio.ensure_future(old_client.aclose())
            changes.append(f"base_url={new_cfg.base_url}")

        for attr in (
            "timeout_first_chunk",
            "timeout_total",
            "explicit_cache_mode",
            "reasoning_effort",
        ):
            old = getattr(self, attr)
            new = getattr(new_cfg, attr)
            if old != new:
                setattr(self, attr, new)
                changes.append(f"{attr}: {old} → {new}")

        self._config = new_cfg
        self._extra_body = dict(new_cfg.extra_body)
        return changes

    # ─── Request Building ─────────────────────────────────────────

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
            resp.raise_for_status()
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
            resp.raise_for_status()
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

                if first_token_ts is None and (content or reasoning):
                    first_token_ts = time.monotonic()

                tcs, deltas = (
                    self._process_tool_deltas(choice, pending)
                    if choice
                    else (None, None)
                )

                decode_tps = 0.0
                if completion_tokens > 0 and first_token_ts is not None:
                    decode_elapsed = time.monotonic() - first_token_ts
                    if decode_elapsed > 0:
                        decode_tps = completion_tokens / decode_elapsed

                yield LLMResponse(
                    content=content,
                    reasoning_content=reasoning,
                    tool_calls=tcs,
                    tool_call_deltas=deltas,
                    usage=LLMUsage(
                        prompt_tokens=prompt_tokens,
                        completion_tokens=completion_tokens,
                        cached_tokens=cached_tokens,
                        first_chunk_rt_ms=first_chunk_rt_ms,
                        tokens_per_sec=decode_tps,
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
