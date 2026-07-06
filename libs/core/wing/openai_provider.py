# wing/openai_provider.py
import asyncio
import json
import time
from typing import AsyncIterator

import httpx
from openai import AsyncOpenAI
from openai.types.chat.chat_completion import ChatCompletion
from openai.types.chat.chat_completion_chunk import Choice

from wing.common.logger import log
from wing.common.with_retry import with_retry
from wing.config import get_config, get_headers
from wing.schema import LLMResponse, LLMUsage, Message, PendingCall, Tool, ToolCall


async def _remove_stainless_headers(request: httpx.Request) -> None:
    """删除 OpenAI SDK 自动添加的 x-stainless-* 头部，避免暴露 Python 特征"""
    for key in list(request.headers.keys()):
        if key.lower().startswith("x-stainless-"):
            del request.headers[key]


class OpenAIProvider:
    def __init__(
        self,
        base_url: str | None = None,
        api_key: str | None = None,
    ):
        config = get_config()
        base_url = base_url or config.openai.base_url
        api_key = api_key or config.openai.api_key

        if not base_url:
            raise ValueError("base_url required (check config.yaml: openai.base_url)")
        if not api_key:
            raise ValueError("api_key required (check config.yaml: openai.api_key)")
        log.info(f"Using open ai base url:{base_url}")
        self.base_url = base_url

        # 构建 default headers（包含 User-Agent 及可能的额外 headers）
        default_headers = get_headers()
        # log.warning(default_headers)

        # 自定义 httpx 客户端，删除 x-stainless-* 头部
        http_client = httpx.AsyncClient(
            event_hooks={"request": [_remove_stainless_headers]}
        )

        self._client = AsyncOpenAI(
            base_url=base_url,
            api_key=api_key,
            default_headers=default_headers,
            http_client=http_client,
        )
        self.thinking = True
        self.reasoning_effort: str | None = config.openai.reasoning_effort
        self.timeout_first_chunk = config.openai.timeout_first_chunk
        self.timeout_total = config.openai.timeout_total
        self.explicit_cache_mode = config.openai.explicit_cache_mode

    @with_retry(max_retries=2)
    async def generate(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None = None,
        stream: bool = False,
    ) -> AsyncIterator[LLMResponse]:
        """统一入口：根据 stream 参数路由到对应实现"""
        log.info(
            f"[BEGIN] openai_provider call {model} with {len(messages)} stream:{stream}"
        )
        openai_messages = [m.to_openai() for m in messages]
        if self.explicit_cache_mode:
            self._apply_cache_control(openai_messages)
        extra_body: dict = {"enable_thinking": self.thinking, "preserve_thinking": True}
        if self.reasoning_effort:
            extra_body["reasoning_effort"] = self.reasoning_effort
        create_params = {
            "model": model,
            "messages": openai_messages,
            "tools": [t.to_openai() for t in tools] if tools else None,
            "stream": stream,
            "extra_body": extra_body,
        }

        generator = (
            self._generate_stream(create_params)
            if stream
            else self._generate_sync(create_params)
        )
        async for item in generator:
            yield item

    async def _generate_sync(self, create_params: dict) -> AsyncIterator[LLMResponse]:
        """非流式：等待完整响应后返回"""
        t0 = time.monotonic()
        try:
            response: ChatCompletion = await asyncio.wait_for(
                self._client.chat.completions.create(**create_params),
                timeout=self.timeout_total,
            )
        except asyncio.TimeoutError:
            log.error(f"LLM request timeout after {self.timeout_total}s")
            raise TimeoutError(f"LLM request timeout after {self.timeout_total}s")
        log.info("[DONE] openai_provider sync call")

        elapsed = time.monotonic() - t0
        message = response.choices[0].message
        tool_calls = [
            ToolCall(
                id=tc.id,
                name=tc.function.name,  # ty: ignore[unresolved-attribute]
                arguments=json.loads(tc.function.arguments),  # ty: ignore[unresolved-attribute]
            )
            for tc in (message.tool_calls or [])
        ]
        if not message and not tool_calls:
            raise RuntimeError("LLM called with unexpected empty response.")

        prompt_tokens = (
            response.usage.prompt_tokens if response.usage is not None else 0
        )
        completion_tokens = (
            response.usage.completion_tokens if response.usage is not None else 0
        )
        cached_tokens = (
            response.usage.prompt_tokens_details.cached_tokens
            if response.usage is not None
            and response.usage.prompt_tokens_details is not None
            else 0
        ) or 0
        yield LLMResponse(
            content=message.content,
            reasoning_content=getattr(message, "reasoning_content", None),
            tool_calls=tool_calls or None,
            usage=LLMUsage(
                prompt_tokens=prompt_tokens,
                completion_tokens=completion_tokens,
                cached_tokens=cached_tokens,
                first_chunk_rt_ms=elapsed * 1000,
                tokens_per_sec=completion_tokens / elapsed if elapsed > 0 else 0.0,
                model=create_params["model"],
            ),
        )

    async def _generate_stream(
        self,
        create_params: dict,
    ) -> AsyncIterator[LLMResponse]:
        """流式：迭代返回 chunk"""
        create_params["stream_options"] = {"include_usage": True}
        t0 = time.monotonic()
        try:
            response = await asyncio.wait_for(
                self._client.chat.completions.create(**create_params),
                timeout=self.timeout_first_chunk,
            )
        except asyncio.TimeoutError:
            log.error(f"LLM first chunk timeout after {self.timeout_first_chunk}s")
            raise TimeoutError(
                f"LLM first chunk timeout after {self.timeout_first_chunk}s"
            )
        log.info("[DONE] openai_provider stream call first chunk")
        first_chunk_rt_ms = (time.monotonic() - t0) * 1000

        pending: dict[int, PendingCall] = {}

        first_token_ts: float | None = None

        async for chunk in response:
            choice = chunk.choices[0] if chunk.choices else None
            delta = choice.delta if choice else None

            prompt_tokens = chunk.usage.prompt_tokens if chunk.usage is not None else 0
            completion_tokens = (
                chunk.usage.completion_tokens if chunk.usage is not None else 0
            )
            cached_tokens = (
                chunk.usage.prompt_tokens_details.cached_tokens
                if chunk.usage is not None
                and chunk.usage.prompt_tokens_details is not None
                else 0
            ) or 0
            reasoning = getattr(delta, "reasoning_content", None)
            content = getattr(delta, "content", None)

            # 首个真正 token 时间
            if first_token_ts is None and (content or reasoning):
                first_token_ts = time.monotonic()

            tcs = (
                [item async for item in self._handle_tool_calls(choice, pending)]
                if choice
                else None
            )

            # Decode TPS
            decode_tps = 0.0

            if completion_tokens > 0 and first_token_ts is not None:
                decode_elapsed = time.monotonic() - first_token_ts

                if decode_elapsed > 0:
                    decode_tps = completion_tokens / decode_elapsed

            yield LLMResponse(
                content=content,
                reasoning_content=reasoning,
                tool_calls=tcs,
                usage=LLMUsage(
                    prompt_tokens=prompt_tokens,
                    completion_tokens=completion_tokens,
                    cached_tokens=cached_tokens,
                    first_chunk_rt_ms=first_chunk_rt_ms,
                    tokens_per_sec=decode_tps,
                    model=create_params["model"],
                ),
            )

    async def _handle_tool_calls(
        self,
        choice: Choice,
        pending: dict[int, PendingCall],  # NOTE: 这里不能用闭包，著名陷阱
    ) -> AsyncIterator[ToolCall]:
        for tc in choice.delta.tool_calls or []:
            call = pending.setdefault(tc.index, PendingCall())
            if tc.id:
                call.id = tc.id
            if tc.function and tc.function.name:
                call.name = tc.function.name
            if tc.function and tc.function.arguments:
                call.args_buffer += tc.function.arguments
        if choice.finish_reason == "tool_calls":
            for call in pending.values():
                yield ToolCall(
                    id=call.id,
                    name=call.name,
                    arguments=json.loads(call.args_buffer),
                )
            pending.clear()

    @staticmethod
    def _apply_cache_control(openai_messages: list[dict]) -> None:
        """为最后一个 content 块追加 cache_control 标记（显式缓存模式）。

        不修改 Message schema，仅在 provider 层对 OpenAI 格式做最后一道转换。
        content 为 str → 转为 content parts 数组；content 已为 list → 在末尾 part追加。
        """
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
            # content 已是 content parts 数组，在最后一个 part 上追加
            last_msg["content"][-1]["cache_control"] = {"type": "ephemeral"}

    def set_thinking(self, enable: bool):
        self.thinking = enable

    def set_reasoning_effort(self, effort: str | None):
        self.reasoning_effort = effort

    def reload(self) -> list[str]:
        """从最新 config 刷新 provider 配置。返回变更项列表。"""
        config = get_config()
        changes: list[str] = []

        # base_url / api_key 变了需要重建 client
        new_url = config.openai.base_url
        new_key = config.openai.api_key
        if new_url != self.base_url or new_key != self._client.api_key:
            self.base_url = new_url
            default_headers = get_headers()
            http_client = httpx.AsyncClient(
                event_hooks={"request": [_remove_stainless_headers]}
            )
            self._client = AsyncOpenAI(
                base_url=new_url,
                api_key=new_key,
                default_headers=default_headers,
                http_client=http_client,
            )
            changes.append(f"base_url={new_url}")

        # 简单属性直接刷新
        for attr in (
            "timeout_first_chunk",
            "timeout_total",
            "explicit_cache_mode",
            "reasoning_effort",
        ):
            old = getattr(self, attr)
            new = getattr(config.openai, attr)
            if old != new:
                setattr(self, attr, new)
                changes.append(f"{attr}: {old} → {new}")

        return changes

    async def list_models(self) -> list[str]:
        """获取可用模型列表"""
        try:
            response = await self._client.models.list()
            return sorted([m.id for m in response.data])
        except Exception as e:
            log.error(f"Failed to list models: {e}")
            raise
