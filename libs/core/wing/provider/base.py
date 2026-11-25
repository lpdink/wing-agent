# wing/provider/base.py
"""ModelProvider ABC — 模型调用层的统一接口。

所有协议实现（OpenAI 兼容、Anthropic）继承此基类，
输出统一的 LLMResponse 流，上层 ReActLoop 对协议无感知。

**无状态契约**：provider 实例共享于全部会话（生命周期归
``wing.provider.pool``），实例上只允许存在「配置 + 连接池」两类状态；
一切会话级参数（缓存亲和的 session id、会话媒体读接口、thinking / effort
开关）经 `RequestOptions` 在**每次调用**时注入。实例的关闭（`retire`）
不打断在途请求：退场只在在途计数归零时真正关闭 client——重试栈因此
不会绑死在一个已被关闭的 client 上（reload 与在途调用的竞态根因）。
"""

from __future__ import annotations

import json
from abc import ABC, abstractmethod
from collections.abc import Mapping
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, AsyncIterator, Sequence, cast

from wing.common.logger import log
from wing.schema import (
    CachePart,
    CacheProbe,
    LLMResponse,
    Message,
    PendingCall,
    Tool,
)

if TYPE_CHECKING:
    from wing.config import ProviderConfig
    from wing.media import MediaAccess


@dataclass(frozen=True)
class RequestOptions:
    """一次模型调用的会话级参数（provider 无状态化的唯一注入点）。

    全部字段可选、全部「None = 不注入」：

    - ``session_id``：缓存亲和的 prompt cache key（explicit_cache_mode 的
      OpenAI 兼容 provider 据此下发 ``prompt_cache_key``；
      Anthropic 走 cache_control，不需要）；
    - ``media``：会话媒体读接口——序列化图片时按 id 读字节；
    - ``thinking`` / ``reasoning_effort``：会话级开关覆盖；None = 跟随
      provider 配置（extra_body / config.reasoning_effort）的默认；
    - ``cache_parts``：ContextManager 产出的 system 侧分段（缓存账本的探针输入）。
      空元组 = 没有 system 侧段落；
    - ``cache_probe``：本次调用的分段累积器。provider 在构建 body 的同一处把
      ``[*cache_parts, *wire 侧段落]`` 写进去。**None = 探针关闭**（一个分段都
      不算）——这正是「开/关探针时请求体逐字节相同」可被测试钉住的开关。
    """

    session_id: str | None = None
    media: MediaAccess | None = None
    thinking: bool | None = None
    reasoning_effort: str | None = None
    cache_parts: tuple[CachePart, ...] = ()
    cache_probe: CacheProbe | None = None


class ProviderClosedError(RuntimeError):
    """已经关闭的 provider 实例被用于发起新调用。

    只应出现在「解析出实例后池恰好换新并关闭了它」的竞态残窗（正常路径
    由「解析后立即迭代」的不变量排除，见 `ReActLoop._call_llm_validated`）。
    立刻失败而不是进重试栈空转——对已关闭的 client 重试永远不可能成功。
    """


@dataclass(frozen=True)
class PendingToolView:
    """未终结 tool call 的渲染投影（活工具卡素材）。

    `args_fragment` 是模型吐出的**原始参数文本**累积——后端 MUST NOT 对其
    做任何 JSON 解析（含 partial parse）；局部解析职责在客户端
    （`util/partial_json.rs`）。与 `snapshot_blocks()` 覆盖互斥：已终结块进
    snapshot，未终结调用进本投影。
    """

    tool_call_id: str
    tool_name: str
    args_fragment: str


def mark_last_cache_control(
    messages: list[dict], *, image_types: frozenset[str]
) -> None:
    """给最后一条消息的最后一个**非图片** content 块打 ``cache_control`` 标记。

    两个协议同构：cache_control 是附加字段，不改写块内容、不影响 thinking 签名；
    落点恒为「最后一个 part；仅当它是图片时回退到其前面最后一个非图片 part」
    ——因此从尾部反向扫描（cache_control 不落在图片上；正向扫描会把标记提前，
    缓存前缀变短）。整条消息没有任何非图片 part（纯图消息）时跳过本次标记。

    ``image_types`` 是唯一的协议知识（OpenAI 兼容用 ``image_url``、Anthropic 用
    ``image``），由各协议的序列化模块传入；``content`` 是字符串的形态（OpenAI 的
    历史写法）由调用方先归一成块数组——Anthropic 侧没有这个形态，不在这里凭空支持。
    """
    if not messages:
        return
    content = messages[-1].get("content")
    if not isinstance(content, list) or not content:
        return
    for part in reversed(cast("list[dict]", content)):
        if part.get("type") in image_types:
            continue
        part["cache_control"] = {"type": "ephemeral"}
        return


def pending_views(pending: Mapping[int, PendingCall]) -> list[PendingToolView]:
    """未终结 tool 调用的渲染投影（两协议共用：args 原文搬运，后端不解析）。

    ``pending`` 是各协议流状态的 per-index 累积表（OpenAI 的 ``state.pending`` /
    Anthropic 的 ``state.pending_tools``）；按 index 定序，无 id 的半截调用跳过
    （投影不出来，截断检测走 ``unfinished_tool_calls`` 的计数口径）。
    """
    views: list[PendingToolView] = []
    for idx in sorted(pending):
        call = pending[idx]
        if not call.id:
            continue
        views.append(
            PendingToolView(
                tool_call_id=call.id,
                tool_name=call.name,
                args_fragment=call.args_buffer,
            )
        )
    return views


def parse_tool_args(raw: str) -> tuple[dict, str | None]:
    """解析工具调用原始 args JSON——永不抛异常。

    笨模型的 args JSON 可能非法（尾逗号、未转义引号等）。解析失败时
    MUST NOT 让异常逃逸出流——那会触发整轮重试、丢弃已生成的
    thinking/content/tool call。正确路径：返回 ({}, 错误现场)，由
    ToolExecutor 短路执行并把错误作为工具结果回灌给模型自纠。

    Returns:
        (arguments, arguments_error)：
        - 成功 → (解析出的 dict, None)
        - 空/空白串 → ({}, None)——无参工具的部分服务端不下发 "{}"
        - 非法 JSON 或非 object → ({}, 错误描述 + 完整原始文本)
    """
    if not raw or not raw.strip():
        return {}, None
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as e:
        return {}, (
            f"Invalid JSON in tool call arguments: {e}. Arguments received:\n{raw}"
        )
    if not isinstance(parsed, dict):
        return {}, (
            f"Tool call arguments must be a JSON object, "
            f"got {type(parsed).__name__}. Arguments received:\n{raw}"
        )
    return parsed, None


class StreamAccumulator:
    """caller 持有的流累积状态容器（provider 协议中立的不透明壳）。

    provider.generate(..., accumulator=...) 在每次尝试开始时把内部流
    状态装入 holder.state（重试重置——累积只反映当前尝试，与消费者看到
    的重传内容一致）。caller 不解读 state，只原样传回
    provider.snapshot_blocks(accumulator)。
    """

    __slots__ = ("state",)

    def __init__(self) -> None:
        self.state: object | None = None


class ModelProvider(ABC):
    """模型调用 provider 基类。"""

    _config: ProviderConfig
    """创建时固化的 provider 配置（只读；配置变更 = 池换新实例）。"""

    def __init__(self) -> None:
        # ── 生命周期（池所有权的支撑状态；与「会话状态」无关）──
        # 在途计数：generate 全程（含协议实现内部的重试）持有，retire 只在
        # 归零时真正关闭 client——退场绝不打断已发出的请求。
        self._inflight: int = 0
        self._retired: bool = False
        self._closed: bool = False

    @property
    def name(self) -> str:
        """Provider 名称（来自 config）。"""
        return self._config.name

    @property
    def protocol(self) -> str:
        """协议类型（来自 config，如 "openai" / "anthropic"）。"""
        return self._config.protocol

    @property
    def config(self) -> "ProviderConfig":
        """创建时固化的 provider 配置（只读）。

        供协作方消费配置口径（如 ReActLoop 的重试参数解析）。
        """
        return self._config

    async def generate(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None = None,
        stream: bool = False,
        accumulator: "StreamAccumulator | None" = None,
        options: "RequestOptions | None" = None,
    ) -> AsyncIterator[LLMResponse]:
        """统一调用入口，产出 LLMResponse 流（模板方法）。

        会话级参数一律经 `options` 注入（provider 实例零会话状态）。

        accumulator：caller 持有的流累积状态容器（可选）。传入时 provider
        在每次尝试开始时填充新状态（重试重置——累积只反映当前尝试）；
        流被取消后 caller 仍可经 snapshot_blocks() 读取已累积的部分内容
        （中断补提交路径）。不传时行为与无 accumulator 完全一致。

        本方法承担两个生命周期不变量（子类经 `_generate` 实现协议细节）：
        在途登记覆盖整个调用（含重试——重试期间实例不会被退场关闭）；
        已关闭实例上的新调用立即失败（`ProviderClosedError`，不进重试栈）。
        """
        call = options or RequestOptions()
        self._inflight += 1
        try:
            if self._closed:
                raise ProviderClosedError(
                    f"provider '{self._config.name}' client has been closed"
                )
            async for item in self._generate(
                messages, model, tools, stream, accumulator, call
            ):
                yield item
        finally:
            self._inflight -= 1
            if self._retired and self._inflight == 0:
                await self.aclose()

    @abstractmethod
    def _generate(
        self,
        messages: list[Message],
        model: str,
        tools: list[Tool] | None = None,
        stream: bool = False,
        accumulator: "StreamAccumulator | None" = None,
        options: "RequestOptions | None" = None,
    ) -> AsyncIterator[LLMResponse]:
        """协议实现：单次 generate 的全部尝试（含 `@with_retry` 重试）。

        声明为非 async def（返回类型即 async generator）——与运行时调用形态
        （async for 消费）一致，也让类型检查器看到正确的可迭代面。
        """
        ...  # pragma: no cover

    @property
    def thinking(self) -> bool:
        """thinking 的配置基线（无会话覆盖时请求会带什么）。

        协议实现按自己的 extra_body 语义派生；会话级覆盖优先于此值
        （见 `WingAgent.thinking`）。基类默认 False。
        """
        return False

    # ── 生命周期（池所有权）───────────────

    async def retire(self) -> None:
        """池换新后的优雅退场：无在途请求立即关闭；有在途则等归零时关闭。

        reload 语义由此保证：在途请求在旧实例上正常跑完（不打断），新请求
        由池解析到新实例（新配置立即生效）。
        """
        self._retired = True
        if self._inflight == 0:
            await self.aclose()

    async def aclose(self) -> None:
        """关闭底层连接（幂等）。

        仅由池在退场收尾 / 进程终结 / 测试中调用——会话与 agent 不持有
        生命周期所有权。
        """
        self._closed = True
        await self._close_transport()

    async def _close_transport(self) -> None:
        """协议实现：释放自己的 httpx client。"""

    # ── 流累积状态（中断补提交）───────────────

    def create_accumulator(self) -> "StreamAccumulator":
        """创建 caller 持有的流累积状态容器。

        基类默认返回不透明容器（无累积语义）。具体 provider 覆盖：
        generate(..., accumulator) 每次尝试填充新状态；caller 在流被
        取消后经 snapshot_blocks() 取已累积的部分内容块。
        """
        return StreamAccumulator()

    def snapshot_blocks(self, accumulator: "StreamAccumulator | None") -> list | None:
        """从累积状态提取已生成的内容块（caller 在取消后调用）。

        语义：text/thinking 块任意长度保留；未被流协议终结的 tool 块
        丢弃（半截参数不可解析，且无配对结果会使下轮请求结构非法）。
        基类默认返回 None（无状态可取）。
        """
        return None

    def pending_tool_calls(
        self, accumulator: "StreamAccumulator | None"
    ) -> list[PendingToolView]:
        """从累积状态提取**未终结**的 tool 调用（活工具卡渲染投影）。

        与 `snapshot_blocks()` 覆盖互斥：snapshot 只含已终结块（未终结 tool
        块被丢弃），本投影补齐那部分——携带原始 args 文本累积，后端不解析。
        用于中途订阅者看到带半截参数的活工具卡。基类默认返回空列表。

        截断检测不走本投影（无 id 的调用投影不出来）——用
        `unfinished_tool_calls()` 计数口径。
        """
        return []

    def unfinished_tool_calls(self, accumulator: "StreamAccumulator | None") -> int:
        """流结束时**未终结**的 tool call 数（截断检测）。

        与 `pending_tool_calls()` 同源但口径更宽：不做投影过滤——无 id 的
        半截调用也计入（检测无盲区）。非零 ⇒ 流被截断（tool 块已开始输出
        但从未收到终结信号），上层据此判定无效轮次。基类默认 0（无累积语义）。
        """
        return 0

    # ── 缓存账本探针（只观察，不改写请求）───────

    def probe_part(self, label: str, obj: Any) -> "CachePart | None":
        """构造一个探针分段；**永不抛**（构造失败只丢这一段）。

        探针是观察面：分段构造（协议 dict → 规范形 JSON）失败绝不能把请求构造
        打断——这是「账本全部工作在 try/except 内」在 provider 侧的落点。
        """
        try:
            return CachePart.obj(label, obj)
        except Exception as e:
            log.warning(f"Cache probe segment failed ({label}): {e}")
            return None

    def publish_cache_segments(
        self, options: "RequestOptions | None", segments: Sequence[CachePart]
    ) -> None:
        """把本次调用的完整分段写进探针（探针关闭 = 零动作、零计算）。

        协议实现在**构建 body 之后**调用：`segments` 必须已经包含 system 侧
        段落（`options.cache_parts`，由 CM 产出）与 wire 侧段落，顺序即 tokenize
        顺序。写入是覆盖式的——重试重放同一请求不会翻倍。
        """
        if options is None or options.cache_probe is None:
            return
        try:
            options.cache_probe.set_segments(segments)
        except Exception as e:
            log.warning(f"Cache probe failed to record segments: {e}")
