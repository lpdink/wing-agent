# wing/provider/openai/provider.py
"""OpenAI 兼容协议 Provider — 基于 httpx 实现。

实现按子包内三层切分：请求期消息序列化在 ``serialize``，流状态与处理器在
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
from wing.config import get_headers
from wing.provider.base import (
    ModelProvider,
    PendingToolView,
    RequestOptions,
    StreamAccumulator,
    parse_tool_args,
)
from wing.provider.openai.serialize import _SerializeMixin
from wing.provider.openai.stream import _OAIStreamState, _StreamMixin
from wing.provider.transport import (
    STREAM_IDLE_TIMEOUT,
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
    Tool,
    ToolCall,
)

if TYPE_CHECKING:
    from wing.config import ProviderConfig


class OpenAICompatProvider(_SerializeMixin, _StreamMixin, ModelProvider):
    """OpenAI 兼容协议 provider（httpx 实现）。

    无状态：实例只含配置与连接池；会话级参数（session id / 媒体池 / 开关）
    经 `generate(..., options=RequestOptions)` 注入（见 ModelProvider）。
    """

    def __init__(self, config: ProviderConfig) -> None:
        super().__init__()
        self._config = config
        self.base_url = config.base_url.rstrip("/")
        self.timeout_first_chunk = config.timeout_first_chunk
        self.timeout_total = config.timeout_total
        self.explicit_cache_mode = config.explicit_cache_mode
        # 深拷贝：配置里的 extra_body 是共享只读源——任何路径都不得写穿它。
        self._extra_body: dict = copy.deepcopy(config.extra_body)
        # enable_thinking / preserve_thinking 默认随每个请求发送（用户 extra_body 的
        # 显式值优先）。preserve_thinking 尤为关键——缺它则多轮工具回合间 thinking
        # 被服务端剥离。thinking 的会话级覆盖在请求期经 options 注入（per-request
        # 视图），与 Anthropic 路径同构。
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
    async def _generate(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None = None,
        stream: bool = False,
        accumulator: StreamAccumulator | None = None,
        options: RequestOptions | None = None,
    ) -> AsyncIterator[LLMResponse]:
        options = options or RequestOptions()
        log.info(
            f"[BEGIN] openai_compat call {model} with {len(messages)} stream:{stream}"
        )
        body = self._build_body(messages, model, tools, stream, options)

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

        已终结的 tool call（finish_reason=tool_calls 时解析入 final_tool_calls）
        保留；仍在 pending（参数流未完成）的调用丢弃——半截参数不可解析。
        状态从未填充（流未开始/非流式）返回 None。
        """
        state = accumulator.state if accumulator is not None else None
        if not isinstance(state, _OAIStreamState):
            return None
        if not (
            state.reasoning_chunks or state.content_chunks or state.final_tool_calls
        ):
            return None
        return self._build_content_blocks(
            "".join(state.reasoning_chunks) or None,
            "".join(state.content_chunks) or None,
            state.final_tool_calls,
        )

    def pending_tool_calls(
        self, accumulator: StreamAccumulator | None
    ) -> list[PendingToolView]:
        """未终结 tool 调用投影——数据源 `state.pending`。

        与 `snapshot_blocks` 判定互斥：snapshot 只取 `final_tool_calls`
        （finish_reason=tool_calls 时解析入列并 clear pending），仍在 `pending`
        中的即未终结调用。`args_fragment` 搬运原始 args 文本累积，后端不解析。
        截断检测走 `unfinished_tool_calls()`（不做 id 过滤的计数口径）。
        """
        state = accumulator.state if accumulator is not None else None
        if not isinstance(state, _OAIStreamState):
            return []
        views: list[PendingToolView] = []
        for idx in sorted(state.pending):
            call = state.pending[idx]
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
        if not isinstance(state, _OAIStreamState):
            return 0
        return len(state.pending)

    async def _close_transport(self) -> None:
        await self._client.aclose()

    @property
    def thinking(self) -> bool:
        """thinking 的**配置基线**——extra_body 的 enable_thinking 派生值。

        会话级覆盖（options.thinking）优先于此值；本 property 是「无覆盖时
        请求会带什么」的口径（会话侧生效值 = 覆盖 ?? 基线，见
        `WingAgent.thinking`）。
        """
        return bool(self._extra_body.get("enable_thinking", True))

    # ─── Request Building ─────────────────────────────────────────

    def _build_body(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None,
        stream: bool,
        options: RequestOptions | None = None,
    ) -> dict:
        options = options or RequestOptions()
        openai_messages = self._serialize_messages(messages, model, options)
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

        # 会话级 thinking 覆盖：只改 per-request 视图，绝不写穿共享 extra_body。
        extra_body = self._extra_body
        if options.thinking is not None:
            extra_body = {**self._extra_body, "enable_thinking": options.thinking}
        for k, v in extra_body.items():
            if k not in body:
                body[k] = v

        effort = (
            options.reasoning_effort
            if options.reasoning_effort is not None
            else self._config.reasoning_effort
        )
        if effort:
            body["reasoning_effort"] = effort

        # prompt_cache_key（显式缓存模式）：key 来自调用方注入的会话 id
        # （缓存亲和的存量行为，option 缺省即不下发）。
        # 已知限制：key 是**本会话**的 session id——fork 出的子会话用自己的
        # id，若上游按 key 隔离缓存，父→子无法复用同一前缀的缓存块
        # （见 docs/dev/architecture.md「压缩与缓存哲学」的已知边界）。
        if self.explicit_cache_mode and options.session_id:
            body["prompt_cache_key"] = options.session_id

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
                arguments=args,
                arguments_error=args_error,
            )
            for tc in (message.get("tool_calls") or [])
            for args, args_error in [parse_tool_args(tc["function"]["arguments"])]
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
                stop_reason=choice.get("finish_reason"),
            ),
        )

    # ─── Streaming ────────────────────────────────────────────────

    async def _generate_stream(
        self, body: dict, model: str, accumulator: StreamAccumulator | None = None
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

        request_id = resp.headers.get("x-request-id", "")
        first_chunk_rt_ms = (time.monotonic() - t0) * 1000
        log.info("[DONE] openai_compat response header received")

        # 累积状态装入 caller 持有的容器（每次尝试重置——重试重传时
        # 快照只反映当前尝试，与消费者看到的内容一致）
        state = _OAIStreamState()
        if accumulator is not None:
            accumulator.state = state

        try:
            async for event in parse_sse_stream(
                lines_with_idle_timeout(
                    resp.aiter_lines(),
                    timeout=STREAM_IDLE_TIMEOUT,
                    context=f"openai_compat {model}",
                )
            ):
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
                    state.reasoning_chunks.append(reasoning)
                if content:
                    state.content_chunks.append(content)

                # 正文 / 思考 / tool 参数增量都是模型解码产出，都要打点：
                # 纯 tool call 响应（无正文无思考）缺了 tool 分支，decode
                # TPS 恒为 0。按原始 delta 判断而非解析产物 deltas——首片
                # 可能只带 id / name、没有 arguments 片段。
                if state.first_token_ts is None and (
                    content or reasoning or delta.get("tool_calls")
                ):
                    state.first_token_ts = time.monotonic()

                # 终止原因（stop / length / tool_calls）——首个非空值生效
                if state.stop_reason is None and choice is not None:
                    state.stop_reason = choice.get("finish_reason")

                tcs, deltas = (
                    self._process_tool_deltas(choice, state.pending)
                    if choice
                    else (None, None)
                )
                if tcs:
                    state.final_tool_calls.extend(tcs)

                decode_tps = 0.0
                if completion_tokens > 0 and state.first_token_ts is not None:
                    decode_elapsed = time.monotonic() - state.first_token_ts
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
            # stop_reason 随最终 usage 传导（截断审计：length ≠ stop）。
            yield LLMResponse(
                content_blocks=self._build_content_blocks(
                    "".join(state.reasoning_chunks) or None,
                    "".join(state.content_chunks) or None,
                    state.final_tool_calls,
                ),
                usage=LLMUsage(
                    first_chunk_rt_ms=first_chunk_rt_ms,
                    model=model,
                    request_id=request_id,
                    stop_reason=state.stop_reason,
                ),
            )
        finally:
            await resp.aclose()
