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
from typing import TYPE_CHECKING, AsyncIterator

import httpx

from wing.common.logger import log
from wing.common.with_retry import with_retry
from wing.provider.base import ModelProvider
from wing.provider.sse import parse_json_event, parse_sse_stream
from wing.schema import (
    LLMResponse,
    LLMUsage,
    Message,
    Tool,
    ToolCall,
    ToolCallDelta,
)

if TYPE_CHECKING:
    from wing.config import ProviderConfig


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
        self.thinking = True
        self.reasoning_effort: str | None = config.reasoning_effort
        self.timeout_first_chunk = config.timeout_first_chunk
        self.timeout_total = config.timeout_total
        self.explicit_cache_mode = config.explicit_cache_mode
        self._extra_body: dict = dict(config.extra_body)
        self._anthropic_version = config.anthropic_version

        headers = {
            "x-api-key": config.api_key,
            "anthropic-version": self._anthropic_version,
            "Content-Type": "application/json",
        }

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
            resp.raise_for_status()
            data = resp.json()
            return sorted([m["id"] for m in data.get("data", [])])
        except Exception as e:
            log.error(f"Failed to list models: {e}")
            raise

    async def aclose(self) -> None:
        await self._client.aclose()

    def set_thinking(self, enable: bool) -> None:
        # Anthropic 协议不通过此 flag 控制 thinking；用户应通过 extra_body 配置。
        # No-op：不持有状态，不影响请求。
        pass

    def set_reasoning_effort(self, effort: str | None) -> None:
        # Anthropic 协议无 reasoning_effort 概念（百炼用 output_config.effort，走 extra_body）。
        # No-op：不持有状态，不影响请求。
        pass

    def reload(self) -> list[str]:
        from wing.config import get_config

        config = get_config()
        new_cfg = None
        for p in config.providers:
            if p.name == self._config.name:
                new_cfg = p
                break
        if new_cfg is None:
            return []

        changes: list[str] = []
        if new_cfg.base_url != self._config.base_url:
            self.base_url = new_cfg.base_url.rstrip("/")
            old_client = self._client
            self._client = httpx.AsyncClient(
                base_url=self.base_url,
                headers={
                    "x-api-key": new_cfg.api_key,
                    "anthropic-version": new_cfg.anthropic_version,
                    "Content-Type": "application/json",
                },
                timeout=httpx.Timeout(
                    connect=30.0,
                    read=new_cfg.timeout_first_chunk,
                    write=30.0,
                    pool=30.0,
                ),
            )
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
                blocks: list[dict] = []
                # thinking content
                if msg.reasoning_content:
                    thinking_block: dict = {
                        "type": "thinking",
                        "thinking": msg.reasoning_content,
                    }
                    if msg.reasoning_signature:
                        thinking_block["signature"] = msg.reasoning_signature
                    blocks.append(thinking_block)
                # text content
                if msg.content:
                    blocks.append({"type": "text", "text": msg.content})
                # tool_use blocks
                if msg.tool_calls:
                    for tc in msg.tool_calls:
                        blocks.append(
                            {
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": tc.arguments,
                            }
                        )
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

        # 合并连续的 user 消息（Anthropic 要求 user/assistant 交替）
        merged = self._merge_consecutive_user(anthropic_msgs)
        return "\n\n".join(system_parts), merged

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
        """为最后一条消息的最后一个 content block 打 cache_control 标记。"""
        if not anthropic_messages:
            return
        last_msg = anthropic_messages[-1]
        content = last_msg.get("content")
        if isinstance(content, list) and content:
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
            resp.raise_for_status()
        except asyncio.TimeoutError:
            log.error(f"LLM request timeout after {self.timeout_total}s")
            raise TimeoutError(f"LLM request timeout after {self.timeout_total}s")

        request_id = resp.headers.get("request-id", "")
        data = resp.json()
        elapsed = time.monotonic() - t0
        log.info("[DONE] anthropic sync call")

        content_blocks = data.get("content", [])
        text_parts: list[str] = []
        reasoning_parts: list[str] = []
        reasoning_signature: str | None = None
        tool_calls: list[ToolCall] = []

        for block in content_blocks:
            btype = block.get("type")
            if btype == "text":
                text_parts.append(block.get("text", ""))
            elif btype == "thinking":
                reasoning_parts.append(block.get("thinking", ""))
                if block.get("signature"):
                    reasoning_signature = block["signature"]
            elif btype == "tool_use":
                tool_calls.append(
                    ToolCall(
                        id=block["id"],
                        name=block["name"],
                        arguments=block.get("input", {}),
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
            reasoning_signature=reasoning_signature,
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
            resp.raise_for_status()
        except asyncio.TimeoutError:
            log.error(f"LLM first chunk timeout after {self.timeout_first_chunk}s")
            raise TimeoutError(
                f"LLM first chunk timeout after {self.timeout_first_chunk}s"
            )

        request_id = resp.headers.get("request-id", "")
        first_chunk_rt_ms = (time.monotonic() - t0) * 1000
        log.info("[DONE] anthropic stream connected")

        first_token_ts: float | None = None
        reasoning_signature: str | None = None
        # 当前活跃的 content block 状态
        current_block_type: str = ""
        current_tool_id: str = ""
        current_tool_name: str = ""
        tool_args_buffer: str = ""
        tool_emitted_len: int = 0

        # 累计 usage
        prompt_tokens = 0
        completion_tokens = 0
        cached_tokens = 0
        cache_creation = 0
        input_source = "start"  # input_tokens 来源：delta（权威）或 start（兜底）

        try:
            async for event in parse_sse_stream(resp.aiter_lines()):
                data = parse_json_event(event)
                if data is None:
                    continue

                event_type = data.get("type", "")

                if event_type == "content_block_start":
                    block = data.get("content_block", {})
                    current_block_type = block.get("type", "")
                    if current_block_type == "tool_use":
                        current_tool_id = block.get("id", "")
                        current_tool_name = block.get("name", "")
                        tool_args_buffer = ""
                        tool_emitted_len = 0

                elif event_type == "content_block_delta":
                    delta = data.get("delta", {})
                    delta_type = delta.get("type", "")

                    if delta_type == "text_delta":
                        text = delta.get("text", "")
                        if first_token_ts is None and text:
                            first_token_ts = time.monotonic()
                        yield LLMResponse(
                            content=text,
                            usage=LLMUsage(
                                first_chunk_rt_ms=first_chunk_rt_ms,
                                model=model,
                                request_id=request_id,
                            ),
                        )

                    elif delta_type == "thinking_delta":
                        thinking = delta.get("thinking", "")
                        if first_token_ts is None and thinking:
                            first_token_ts = time.monotonic()
                        yield LLMResponse(
                            reasoning_content=thinking,
                            usage=LLMUsage(
                                first_chunk_rt_ms=first_chunk_rt_ms,
                                model=model,
                                request_id=request_id,
                            ),
                        )

                    elif delta_type == "signature_delta":
                        # Anthropic thinking block signature（多轮回放必需）
                        sig = delta.get("signature", "")
                        if sig:
                            reasoning_signature = sig

                    elif delta_type == "input_json_delta":
                        partial_json = delta.get("partial_json", "")
                        tool_args_buffer += partial_json
                        fragment = tool_args_buffer[tool_emitted_len:]
                        if fragment and current_tool_id:
                            tool_emitted_len = len(tool_args_buffer)
                            yield LLMResponse(
                                tool_call_deltas=[
                                    ToolCallDelta(
                                        id=current_tool_id,
                                        name=current_tool_name,
                                        args_fragment=fragment,
                                        is_final=False,
                                    )
                                ],
                                usage=LLMUsage(
                                    first_chunk_rt_ms=first_chunk_rt_ms,
                                    model=model,
                                    request_id=request_id,
                                ),
                            )

                elif event_type == "content_block_stop":
                    if current_block_type == "tool_use" and current_tool_id:
                        # 发射 final tool call
                        try:
                            args = (
                                json.loads(tool_args_buffer) if tool_args_buffer else {}
                            )
                        except json.JSONDecodeError:
                            log.warning(
                                f"Failed to parse tool args JSON for "
                                f"{current_tool_name}: {tool_args_buffer[:200]}"
                            )
                            args = {}
                        # 发射 is_final delta
                        remaining = tool_args_buffer[tool_emitted_len:]
                        deltas = []
                        if remaining:
                            deltas.append(
                                ToolCallDelta(
                                    id=current_tool_id,
                                    name=current_tool_name,
                                    args_fragment=remaining,
                                    is_final=True,
                                )
                            )
                        else:
                            deltas.append(
                                ToolCallDelta(
                                    id=current_tool_id,
                                    name=current_tool_name,
                                    args_fragment="",
                                    is_final=True,
                                )
                            )
                        yield LLMResponse(
                            tool_calls=[
                                ToolCall(
                                    id=current_tool_id,
                                    name=current_tool_name,
                                    arguments=args,
                                )
                            ],
                            tool_call_deltas=deltas,
                            usage=LLMUsage(
                                first_chunk_rt_ms=first_chunk_rt_ms,
                                model=model,
                                request_id=request_id,
                            ),
                        )
                    current_block_type = ""

                elif event_type == "message_delta":
                    # 权威累计 usage（官方文档：message_delta 的 usage 为 cumulative，
                    # 且当前版本 API 含 input_tokens；message_start 仅作兜底）。
                    usage_delta = data.get("usage", {})
                    log.debug(f"[anthropic usage] message_delta raw: {usage_delta}")
                    if "input_tokens" in usage_delta:
                        prompt_tokens = usage_delta["input_tokens"]
                        input_source = "delta"
                    if "output_tokens" in usage_delta:
                        completion_tokens = usage_delta["output_tokens"]
                    if "cache_read_input_tokens" in usage_delta:
                        cached_tokens = usage_delta["cache_read_input_tokens"]
                    if "cache_creation_input_tokens" in usage_delta:
                        cache_creation = usage_delta["cache_creation_input_tokens"]

                elif event_type == "message_start":
                    # 兜底：部分 API 版本（如 2023-06-01）message_delta 不带
                    # input_tokens，此时回退到 message_start 的初值。
                    msg = data.get("message", {})
                    usage_start = msg.get("usage", {})
                    log.debug(f"[anthropic usage] message_start raw: {usage_start}")
                    if "input_tokens" in usage_start:
                        prompt_tokens = usage_start["input_tokens"]
                        input_source = "start"
                    if "cache_read_input_tokens" in usage_start:
                        cached_tokens = usage_start["cache_read_input_tokens"]

                elif event_type == "message_stop":
                    # 最终 usage + signature 发射
                    # Anthropic 的 input_tokens 仅为非缓存部分（含兜底/增量源）；
                    # 对齐 OpenAI 语义：prompt_tokens = 总输入（含缓存）
                    total_prompt = prompt_tokens + cached_tokens + cache_creation
                    hit_rate = (
                        cached_tokens / total_prompt * 100 if total_prompt else 0.0
                    )
                    log.debug(
                        f"[anthropic usage] final: "
                        f"input(non-cached)={prompt_tokens}({input_source}) "
                        f"cache_read={cached_tokens} "
                        f"cache_creation={cache_creation} "
                        f"output={completion_tokens} => "
                        f"prompt_tokens(total)={total_prompt} "
                        f"hit_rate={hit_rate:.1f}%"
                    )
                    decode_tps = 0.0
                    if completion_tokens > 0 and first_token_ts is not None:
                        decode_elapsed = time.monotonic() - first_token_ts
                        if decode_elapsed > 0:
                            decode_tps = completion_tokens / decode_elapsed
                    yield LLMResponse(
                        reasoning_signature=reasoning_signature,
                        usage=LLMUsage(
                            prompt_tokens=total_prompt,
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
