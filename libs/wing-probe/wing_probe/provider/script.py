"""剧本模型 —— 假 Provider 的确定性输出定义（design D3）。

一个 ``Script`` = 一串按序消费的 ``Turn``；每个 ``Turn`` 描述这一轮模型
"吐出什么"：thinking / text / tool_calls / usage / finish_reason，以及
分片粒度（``chunk``）、片间延迟（``delay``）与 tool 参数 JSON 的中间切断点
（``ToolCall.cut``——"半截参数"场景的素材）。

剧本按 **model 名** 注册（``ScriptRegistry``），每次入站请求消费一个 Turn。
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Any

import json

#: 缺省 tool call id 模板（``call_<turn_index>_<call_index>``，跨剧本唯一）。
CALL_ID_TEMPLATE = "call_{turn_index}_{call_index}"


def resolve_call_id(
    call: ToolCall,
    *,
    turn_index: int,
    call_index: int,
) -> str:
    """ToolCall 的 wire id：显式指定优先，否则按剧本位置生成（确定性）。"""
    if call.id:
        return call.id
    return CALL_ID_TEMPLATE.format(turn_index=turn_index, call_index=call_index)


def split_text(text: str | None, size: int | None) -> list[str]:
    """把文本按 ``size`` 字符切片（``None`` / 过大 → 整段一帧）。"""
    if not text:
        return []
    if size is None or size >= len(text):
        return [text]
    return [text[i : i + size] for i in range(0, len(text), size)]


@dataclass(frozen=True)
class Usage:
    """一轮的 usage 数值（假 Provider 是压缩阈值的驱动源）。"""

    prompt_tokens: int = 0
    completion_tokens: int = 0
    cached_tokens: int = 0

    def to_wire(self) -> dict[str, Any]:
        """OpenAI 兼容 usage 载荷（含 cached_tokens 明细）。"""
        return {
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + self.completion_tokens,
            "prompt_tokens_details": {"cached_tokens": self.cached_tokens},
        }


@dataclass(frozen=True)
class ToolCall:
    """一次工具调用：``ToolCall("Bash", {"command": "ls"}, cut=5)``。

    - ``arguments``: dict → 序列化为 JSON；str → 原样使用（可造非法 JSON）；
    - ``cut``: 参数 JSON 的切断点（字符偏移，可多点）——流式时在切断处分成
      多个 ``function.arguments`` 增量帧，拼接仍等于完整参数（非流式恒为
      完整参数，切断只影响流式分帧）；
    - ``id``: 显式 call id；缺省按剧本位置生成（见 ``resolve_call_id``）。
    """

    name: str
    arguments: dict[str, Any] | str
    id: str | None = None
    cut: int | Sequence[int] | None = None

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("ToolCall.name must not be empty")
        points = self.cut_points
        if not points:
            return
        text = self.argument_text
        if list(points) != sorted(set(points)):
            raise ValueError(
                f"ToolCall.cut must be strictly increasing, got {list(points)}"
            )
        if points[0] < 1 or points[-1] >= len(text):
            raise ValueError(
                f"ToolCall.cut must be within 1..{len(text) - 1} for "
                f"arguments {text!r}, got {list(points)}"
            )

    @property
    def argument_text(self) -> str:
        """完整参数文本（dict 序列化为 JSON，str 原样）。"""
        if isinstance(self.arguments, str):
            return self.arguments
        return json.dumps(self.arguments, ensure_ascii=False)

    @property
    def cut_points(self) -> tuple[int, ...]:
        if self.cut is None:
            return ()
        if isinstance(self.cut, int):
            return (self.cut,)
        return tuple(self.cut)

    def argument_chunks(self) -> list[str]:
        """参数文本按切断点切分（无切断点 → 单帧完整参数）。"""
        text = self.argument_text
        points = self.cut_points
        if not points:
            return [text]
        bounds = [0, *points, len(text)]
        return [text[start:end] for start, end in zip(bounds, bounds[1:])]


@dataclass
class Turn:
    """剧本中的一轮：模型这一轮吐什么。

    ``finish`` 为 None 时自动判定（有 tool_calls → ``tool_calls``，否则 ``stop``）。
    ``chunk`` 是 thinking / text 的分片粒度（字符数）；``delay`` 是分片之间的
    等待秒数（首帧不等待——服务端要尽快把响应头交出去）。
    """

    thinking: str | None = None
    text: str | None = None
    tool_calls: list[ToolCall] = field(default_factory=list)
    usage: Usage | None = None
    finish: str | None = None
    chunk: int | None = None
    delay: float = 0.0

    def __post_init__(self) -> None:
        self.tool_calls = list(self.tool_calls)
        if self.chunk is not None and self.chunk < 1:
            raise ValueError(f"Turn.chunk must be >= 1 or None, got {self.chunk}")
        if self.delay < 0:
            raise ValueError(f"Turn.delay must be >= 0, got {self.delay}")

    @classmethod
    def of(
        cls,
        *,
        thinking: str | None = None,
        text: str | None = None,
        tool_calls: Sequence[ToolCall] = (),
        usage: Usage | None = None,
        finish: str | None = None,
        chunk: int | None = None,
        delay: float = 0.0,
    ) -> Turn:
        """关键字形式构造（可读性优先，与 design 示例一致）。"""
        return cls(
            thinking=thinking,
            text=text,
            tool_calls=list(tool_calls),
            usage=usage,
            finish=finish,
            chunk=chunk,
            delay=delay,
        )

    @property
    def finish_reason(self) -> str:
        if self.finish is not None:
            return self.finish
        return "tool_calls" if self.tool_calls else "stop"

    @property
    def resolved_usage(self) -> Usage:
        """未指定 usage 时为全零（确定性优先：不发明数字）。"""
        return self.usage if self.usage is not None else Usage()

    def reasoning_chunks(self) -> list[str]:
        return split_text(self.thinking, self.chunk)

    def content_chunks(self) -> list[str]:
        return split_text(self.text, self.chunk)

    def describe(self) -> str:
        parts: list[str] = []
        if self.thinking is not None:
            parts.append(f"thinking={self.thinking!r}")
        if self.text is not None:
            parts.append(f"text={self.text!r}")
        if self.tool_calls:
            calls = ", ".join(f"{c.name}({c.argument_text})" for c in self.tool_calls)
            parts.append(f"tool_calls=[{calls}]")
        parts.append(f"finish={self.finish_reason}")
        if self.usage is not None:
            parts.append(
                f"usage=({self.usage.prompt_tokens}in/"
                f"{self.usage.completion_tokens}out/"
                f"{self.usage.cached_tokens}cached)"
            )
        if self.chunk is not None:
            parts.append(f"chunk={self.chunk}")
        if self.delay:
            parts.append(f"delay={self.delay}")
        return f"Turn({', '.join(parts)})"


# ── 剧本与路由 ──────────────────────────────────────────────────


class ScriptError(RuntimeError):
    """剧本无法产出下一轮（未注册模型 / 剧本耗尽）——假 Provider 以此 5xx 报告。"""

    code = "probe_script_error"


class UnregisteredModelError(ScriptError):
    """请求的 model 没有注册剧本。"""

    code = "unregistered_model"

    def __init__(self, model: str, registered: Sequence[str]) -> None:
        self.model = model
        self.registered = tuple(registered)
        known = ", ".join(self.registered) if self.registered else "<none>"
        super().__init__(
            f"model {model!r} is not registered with the probe fake provider "
            f"(registered models: {known})"
        )


class ScriptExhaustedError(ScriptError):
    """同一 model 的请求数超过剧本 Turn 数。"""

    code = "script_exhausted"

    def __init__(self, model: str, consumed: int, total: int) -> None:
        self.model = model
        self.consumed = consumed
        self.total = total
        super().__init__(
            f"script for model {model!r} is exhausted: "
            f"consumed {consumed}/{total} turns"
        )


class Script:
    """一串按序消费的 Turn（``Script(Turn.of(...), Turn.of(...))``）。"""

    def __init__(self, *turns: Turn) -> None:
        if not turns:
            raise ValueError("a Script needs at least one Turn")
        self._turns: list[Turn] = list(turns)
        self._consumed = 0

    @property
    def turns(self) -> list[Turn]:
        """全部 Turn（副本）。"""
        return list(self._turns)

    @property
    def consumed(self) -> int:
        return self._consumed

    @property
    def remaining(self) -> int:
        return len(self._turns) - self._consumed

    def consume(self, *, model: str = "") -> tuple[Turn, int]:
        """消费下一个 Turn，返回 ``(turn, turn_index)``（0-based）。"""
        if self._consumed >= len(self._turns):
            raise ScriptExhaustedError(model, self._consumed, len(self._turns))
        index = self._consumed
        self._consumed += 1
        return self._turns[index], index

    def reset(self) -> None:
        """消费游标归零（同一剧本可重放）。"""
        self._consumed = 0

    def __len__(self) -> int:
        return len(self._turns)

    def __repr__(self) -> str:
        return f"Script({len(self._turns)} turns, consumed={self._consumed})"


class ScriptRegistry:
    """``model 名 → Script`` 路由表（每次请求按序消费）。"""

    def __init__(self) -> None:
        self._scripts: dict[str, Script] = {}

    def register(self, model: str, script: Script) -> Script:
        """注册剧本；同一 model 重复注册抛 ``ValueError``（防静默覆盖）。"""
        if not model:
            raise ValueError("model name must not be empty")
        if model in self._scripts:
            raise ValueError(
                f"model {model!r} already has a script registered; "
                "use reset() on the existing Script or pick another model name"
            )
        self._scripts[model] = script
        return script

    def get(self, model: str) -> Script:
        try:
            return self._scripts[model]
        except KeyError:
            raise UnregisteredModelError(model, self.models()) from None

    def consume(self, model: str) -> tuple[Turn, int]:
        """路由并消费：未注册 / 耗尽分别抛 ``ScriptError`` 子类。"""
        return self.get(model).consume(model=model)

    def models(self) -> list[str]:
        return sorted(self._scripts)

    def clear(self) -> None:
        self._scripts.clear()

    def __contains__(self, model: object) -> bool:
        return model in self._scripts

    def __len__(self) -> int:
        return len(self._scripts)

    def describe(self) -> str:
        if not self._scripts:
            return "no scripts registered"
        return "\n".join(
            f"  {model}: {script!r}" for model, script in sorted(self._scripts.items())
        )


__all__ = [
    "CALL_ID_TEMPLATE",
    "Script",
    "ScriptError",
    "ScriptExhaustedError",
    "ScriptRegistry",
    "ToolCall",
    "Turn",
    "UnregisteredModelError",
    "Usage",
    "resolve_call_id",
    "split_text",
]
