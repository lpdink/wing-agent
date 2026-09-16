"""ContextView —— 入站 LLM 请求的规范化视图与上下文断言（design D3）。

规范化维度：``system`` / ``messages``（role、content 扁平化、结构化 tool_calls、
tool_call_id）/ ``tool_names``。消息保留**请求体下标**（``MessageView.index``），
失败报告直接给"第几条消息、哪个 call_id"，不用回翻原始 JSON。

断言：
- ``assert_tool_pairing()``：每个 assistant tool_call 有配对 tool 消息、无孤儿；
- ``assert_prefix_like([...])``：消息前缀语义等价；
- ``assert_tail_from([...])``：消息尾部（保留区）未丢；
- ``role_sequence()``：角色序列（查询，不推进任何状态）。
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any, cast

#: role 视作 system（不进入 ``messages``）的取值。
SYSTEM_ROLES: frozenset[str] = frozenset({"system", "developer"})

#: dict 规格（``MsgSpec``）允许的键；未知键报错而非静默通过。
SPEC_KEYS: frozenset[str] = frozenset(
    {
        "role",
        "content",
        "tool_call_id",
        "name",
        "reasoning_content",
        "has_tool_calls",
        "tool_calls",
    }
)


class ContextAssertionError(AssertionError):
    """上下文断言失败（报告含消息下标与 call_id）。"""


def normalize_text(text: str) -> str:
    """比较用归一化：CRLF → LF，去首尾空白。"""
    return text.replace("\r\n", "\n").strip()


def flatten_content(content: Any) -> str:
    """content 扁平化为文本（兼容 str 与 Anthropic / cache_control 块数组）。"""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for block in content:
            if isinstance(block, str):
                parts.append(block)
            elif isinstance(block, Mapping):
                text = block.get("text")
                if isinstance(text, str):
                    parts.append(text)
                elif isinstance(block.get("content"), str):
                    parts.append(str(block["content"]))
        return "\n".join(parts)
    return str(content)


def truncate(text: str, limit: int) -> str:
    """超长截断（附原始长度，报告可读且不淹没上下文）。"""
    if len(text) <= limit:
        return text
    return f"{text[:limit]}…(+{len(text) - limit} chars)"


@dataclass(frozen=True)
class ToolCallView:
    """一条结构化 tool_call（id / name / 原始 arguments / 解析结果）。"""

    index: int
    id: str
    name: str
    arguments: str
    args: dict[str, Any] | None = None
    error: str | None = None
    """arguments 不是合法 JSON 时的原因（``args`` 为 None）。"""

    def summary(self, *, limit: int = 80) -> str:
        if self.args is not None:
            body = json.dumps(self.args, ensure_ascii=False)
        else:
            body = f"<unparsable JSON: {self.arguments!r} ({self.error})>"
        return f"{self.id} {self.name}({truncate(body, limit)})"

    def __str__(self) -> str:
        return self.summary()


@dataclass(frozen=True)
class MessageView:
    """一条规范化消息（``index`` 是请求体 messages 数组下标）。"""

    index: int
    role: str
    content: str
    tool_calls: tuple[ToolCallView, ...] = ()
    tool_call_id: str | None = None
    name: str | None = None
    reasoning_content: str | None = None
    raw: Mapping[str, Any] = field(default_factory=dict)

    @property
    def call_ids(self) -> tuple[str, ...]:
        return tuple(call.id for call in self.tool_calls)

    @property
    def text_line(self) -> str:
        """``"role: content"`` 形态（content 已按比较口径归一化）。"""
        return f"{self.role}: {normalize_text(self.content)}"

    def summary(self, *, limit: int = 120) -> str:
        extras: list[str] = []
        if self.tool_call_id is not None:
            extras.append(f"tool_call_id={self.tool_call_id}")
        if self.name is not None:
            extras.append(f"name={self.name}")
        if self.tool_calls:
            calls = ", ".join(call.summary() for call in self.tool_calls)
            extras.append(f"tool_calls=[{calls}]")
        if self.reasoning_content:
            extras.append(f"reasoning={truncate(self.reasoning_content, limit)!r}")
        line = f"{self.role}: {truncate(self.content, limit)!r}"
        if extras:
            line = f"{line} {' '.join(extras)}"
        return line


#: 期望消息规格：字符串（``"role: content"``）/ dict（字段子集）/ MessageView。
MsgSpec = str | Mapping[str, Any] | MessageView


def _parse_args(arguments: str) -> tuple[dict[str, Any] | None, str | None]:
    try:
        parsed = json.loads(arguments)
    except (json.JSONDecodeError, ValueError) as exc:
        return None, str(exc)
    if not isinstance(parsed, dict):
        return None, f"expected a JSON object, got {type(parsed).__name__}"
    return parsed, None


def _as_mapping(value: Any) -> Mapping[str, Any] | None:
    """dict 态检查 + 类型收窄。

    ``isinstance(x, Mapping)`` 在 ty 下收窄成 ``Mapping[Unknown, object]``（键类型
    不变），取键会报错——统一走这里显式收窄。
    """
    if isinstance(value, Mapping):
        return cast("Mapping[str, Any]", value)
    return None


def _tool_call_views(raw_message: Mapping[str, Any]) -> tuple[ToolCallView, ...]:
    calls = raw_message.get("tool_calls")
    if not isinstance(calls, list):
        return ()
    views: list[ToolCallView] = []
    for index, entry in enumerate(calls):
        entry_map = _as_mapping(entry)
        if entry_map is None:
            continue
        function = _as_mapping(entry_map.get("function")) or {}
        arguments = function.get("arguments")
        arguments_text = (
            arguments
            if isinstance(arguments, str)
            else json.dumps(
                arguments if arguments is not None else {}, ensure_ascii=False
            )
        )
        args, error = _parse_args(arguments_text)
        views.append(
            ToolCallView(
                index=index,
                id=str(entry_map.get("id") or ""),
                name=str(function.get("name") or ""),
                arguments=arguments_text,
                args=args,
                error=error,
            )
        )
    return tuple(views)


def _tool_names(body: Mapping[str, Any]) -> list[str]:
    tools = body.get("tools")
    if not isinstance(tools, list):
        return []
    names: list[str] = []
    for tool in tools:
        tool_map = _as_mapping(tool)
        if tool_map is None:
            continue
        function = _as_mapping(tool_map.get("function"))
        if function is not None and isinstance(function.get("name"), str):
            names.append(str(function["name"]))
        elif isinstance(tool_map.get("name"), str):
            names.append(str(tool_map["name"]))
    return names


def match_message(message: MessageView, spec: MsgSpec) -> str | None:
    """消息与规格是否匹配；匹配返回 None，否则返回差异说明。"""
    if isinstance(spec, MessageView):
        if normalize_text(spec.content) != normalize_text(message.content):
            return f"content differs: expected {spec.content!r}"
        if spec.role != message.role:
            return f"role differs: expected {spec.role!r}"
        if spec.call_ids != message.call_ids:
            return (
                f"tool_call ids differ: expected {spec.call_ids} got {message.call_ids}"
            )
        if spec.tool_call_id != message.tool_call_id:
            return (
                f"tool_call_id differs: expected {spec.tool_call_id!r} "
                f"got {message.tool_call_id!r}"
            )
        return None

    if isinstance(spec, str):
        if normalize_text(spec) == normalize_text(message.text_line):
            return None
        role, separator, content = spec.partition(":")
        if not separator:
            raise ValueError(
                f"string message spec must be 'role: content', got {spec!r}"
            )
        if role.strip() != message.role:
            return f"role differs: expected {role.strip()!r} got {message.role!r}"
        if normalize_text(content) != normalize_text(message.content):
            return (
                f"content differs: expected {normalize_text(content)!r} "
                f"got {normalize_text(message.content)!r}"
            )
        return None

    if isinstance(spec, Mapping):
        _validate_message_spec(spec)
        if "role" in spec and spec["role"] != message.role:
            return f"role differs: expected {spec['role']!r} got {message.role!r}"
        if "content" in spec and normalize_text(str(spec["content"])) != normalize_text(
            message.content
        ):
            return f"content differs: expected {str(spec['content'])!r}"
        if "tool_call_id" in spec and spec["tool_call_id"] != message.tool_call_id:
            return (
                f"tool_call_id differs: expected {spec['tool_call_id']!r} "
                f"got {message.tool_call_id!r}"
            )
        if "name" in spec and spec["name"] != message.name:
            return f"name differs: expected {spec['name']!r} got {message.name!r}"
        if "reasoning_content" in spec and (message.reasoning_content or "") != str(
            spec["reasoning_content"]
        ):
            return "reasoning_content differs"
        if "has_tool_calls" in spec and bool(spec["has_tool_calls"]) != bool(
            message.tool_calls
        ):
            return (
                f"has_tool_calls differs: expected {bool(spec['has_tool_calls'])}, "
                f"got {bool(message.tool_calls)}"
            )
        if "tool_calls" in spec:
            expected_calls = spec["tool_calls"]
            actual = message.tool_calls
            if len(expected_calls) != len(actual):
                return (
                    f"tool_calls count differs: expected {len(expected_calls)}, "
                    f"got {len(actual)} ({[c.id for c in actual]})"
                )
            for expected_call, actual_call in zip(expected_calls, actual):
                reason = _match_tool_call(actual_call, expected_call)
                if reason is not None:
                    return (
                        f"tool_call[{actual_call.index}] ({actual_call.id}): {reason}"
                    )
        return None

    raise ValueError(f"unsupported message spec type: {type(spec).__name__}")


def _validate_message_spec(spec: Mapping[str, Any]) -> None:
    """规格键必须先过校验（拼错的键不许静默通过，哪怕是"早退"分支）。"""
    unknown = set(spec) - SPEC_KEYS
    if unknown:
        raise ValueError(
            f"unknown message spec key(s) {sorted(unknown)}; "
            f"allowed: {sorted(SPEC_KEYS)}"
        )
    calls = spec.get("tool_calls")
    if calls is None:
        return
    if not isinstance(calls, Sequence) or isinstance(calls, (str, bytes)):
        raise ValueError("tool_calls spec must be a sequence")
    for entry in calls:
        if isinstance(entry, Mapping):
            unknown_call_keys = set(entry) - {"id", "name", "arguments", "args"}
            if unknown_call_keys:
                raise ValueError(
                    f"unknown tool_call spec key(s) {sorted(unknown_call_keys)}; "
                    "allowed: ['args', 'arguments', 'id', 'name']"
                )


def _match_tool_call(call: ToolCallView, spec: Any) -> str | None:
    if isinstance(spec, ToolCallView):
        if spec.name != call.name:
            return f"name differs: expected {spec.name!r} got {call.name!r}"
        if normalize_text(spec.arguments) != normalize_text(call.arguments):
            return (
                f"arguments differ: expected {spec.arguments!r} got {call.arguments!r}"
            )
        if spec.id and spec.id != call.id:
            return f"id differs: expected {spec.id!r} got {call.id!r}"
        return None
    if isinstance(spec, str):
        if spec != call.name:
            return f"name differs: expected {spec!r} got {call.name!r}"
        return None
    if isinstance(spec, Mapping):
        unknown = set(spec) - {"id", "name", "arguments", "args"}
        if unknown:
            raise ValueError(
                f"unknown tool_call spec key(s) {sorted(unknown)}; "
                "allowed: ['args', 'arguments', 'id', 'name']"
            )
        if "id" in spec and spec["id"] != call.id:
            return f"id differs: expected {spec['id']!r} got {call.id!r}"
        if "name" in spec and spec["name"] != call.name:
            return f"name differs: expected {spec['name']!r} got {call.name!r}"
        if "arguments" in spec:
            expected = spec["arguments"]
            text = (
                expected
                if isinstance(expected, str)
                else json.dumps(expected, ensure_ascii=False)
            )
            if normalize_text(text) != normalize_text(call.arguments):
                return f"arguments differ: expected {text!r} got {call.arguments!r}"
        if "args" in spec and spec["args"] != call.args:
            return f"args differ: expected {spec['args']!r} got {call.args!r}"
        return None
    raise ValueError(f"unsupported tool_call spec type: {type(spec).__name__}")


class ContextView:
    """一次 LLM 请求的上下文视图（``body`` = 原始请求体）。"""

    def __init__(self, body: Mapping[str, Any], *, source: str | None = None) -> None:
        self.body: Mapping[str, Any] = body
        self.source = source
        self.model: str | None = (
            body.get("model") if isinstance(body.get("model"), str) else None
        )
        self.stream: bool = bool(body.get("stream"))
        self.tool_names: list[str] = _tool_names(body)

        self.all_messages: list[MessageView] = []
        self.messages: list[MessageView] = []
        """非 system 消息（``index`` 保留请求体下标）。"""
        system_parts: list[str] = []
        raw_messages = body.get("messages")
        if isinstance(raw_messages, list):
            for index, raw_entry in enumerate(raw_messages):
                raw = _as_mapping(raw_entry)
                if raw is None:
                    continue
                role = str(raw.get("role") or "")
                content = flatten_content(raw.get("content"))
                tool_call_id = raw.get("tool_call_id")
                name = raw.get("name")
                reasoning = raw.get("reasoning_content")
                view = MessageView(
                    index=index,
                    role=role,
                    content=content,
                    tool_calls=_tool_call_views(raw),
                    tool_call_id=(
                        str(tool_call_id) if isinstance(tool_call_id, str) else None
                    ),
                    name=str(name) if isinstance(name, str) else None,
                    reasoning_content=(
                        str(reasoning) if isinstance(reasoning, str) else None
                    ),
                    raw=raw,
                )
                self.all_messages.append(view)
                if role in SYSTEM_ROLES:
                    system_parts.append(content)
                else:
                    self.messages.append(view)
        self.system: str | None = "\n\n".join(system_parts) if system_parts else None
        self.raw = body

    # ── 查询 ────────────────────────────────────────────────

    def role_sequence(self) -> list[str]:
        """非 system 消息的角色序列。"""
        return [message.role for message in self.messages]

    def messages_of(self, role: str) -> list[MessageView]:
        return [message for message in self.messages if message.role == role]

    def texts(self, role: str) -> list[str]:
        """某角色全部消息的 content（顺序保留）。"""
        return [message.content for message in self.messages_of(role)]

    def describe(self, *, limit: int = 20) -> str:
        """上下文摘要（失败报告的数据源）。"""
        lines: list[str] = []
        if self.source:
            lines.append(f"source: {self.source}")
        lines.append(
            f"model={self.model!r} stream={self.stream} "
            f"messages={len(self.all_messages)} tools={self.tool_names}"
        )
        if self.system is not None:
            lines.append(f"  system: {truncate(self.system, 200)!r}")
        for message in self.all_messages[:limit]:
            lines.append(f"  [{message.index}] {message.summary()}")
        if len(self.all_messages) > limit:
            lines.append(f"  … {len(self.all_messages) - limit} more message(s)")
        return "\n".join(lines)

    def _failure(self, title: str, problems: Sequence[str]) -> str:
        where = f" [{self.source}]" if self.source else ""
        body = "\n".join(f"  - {problem}" for problem in problems)
        return (
            f"{title}{where} ({len(problems)} problem(s)):\n{body}\n"
            f"context:\n{self.describe()}"
        )

    # ── 断言 ────────────────────────────────────────────────

    def assert_tool_pairing(self, *, require_json_args: bool = True) -> None:
        """tool 配对不变量：每个 tool_call 有配对 tool 消息，且无孤儿。

        ``require_json_args`` 同时要求 assistant 侧参数是合法 JSON 对象
        （半截参数不该出现在请求里）。
        """
        problems: list[str] = []
        calls: dict[str, tuple[int, str]] = {}
        paired: dict[str, int] = {}
        for message in self.messages:
            if message.role == "tool":
                call_id = message.tool_call_id
                if call_id is None:
                    problems.append(
                        f"tool message[{message.index}]: missing tool_call_id"
                    )
                elif call_id not in calls:
                    problems.append(
                        f"tool message[{message.index}] tool_call_id={call_id!r}: "
                        "orphan (no preceding assistant tool_call)"
                    )
                elif call_id in paired:
                    problems.append(
                        f"tool message[{message.index}] tool_call_id={call_id!r}: "
                        f"duplicate (already paired at message[{paired[call_id]}])"
                    )
                else:
                    paired[call_id] = message.index
            for call in message.tool_calls:
                key = call.id or f"<no-id#{message.index}.{call.index}>"
                if key in calls:
                    problems.append(
                        f"assistant message[{message.index}] call {key!r}: "
                        "duplicate call_id"
                    )
                calls[key] = (message.index, call.name)
                if require_json_args and call.args is None:
                    problems.append(
                        f"assistant message[{message.index}] call {key!r} "
                        f"({call.name}): arguments are not a valid JSON object: "
                        f"{call.arguments!r} ({call.error})"
                    )
        for call_id, (assistant_index, name) in calls.items():
            if call_id not in paired:
                problems.append(
                    f"assistant message[{assistant_index}] call {call_id!r} ({name}): "
                    f"missing paired tool message with tool_call_id={call_id!r}"
                )
        if problems:
            raise ContextAssertionError(
                self._failure("tool pairing violated", problems)
            )

    def assert_prefix_like(self, expected: Sequence[MsgSpec]) -> None:
        """消息前缀语义等价（只比给定的前 N 条）。"""
        spec = list(expected)
        actual = self.messages[: len(spec)]
        problems = self._compare(spec, actual, kind="prefix")
        if problems:
            raise ContextAssertionError(self._failure("prefix mismatch", problems))

    def assert_tail_from(self, expected: Sequence[MsgSpec]) -> None:
        """消息尾部（保留区）未丢——末尾 N 条与期望等价。"""
        spec = list(expected)
        problems: list[str] = []
        if len(self.messages) < len(spec):
            problems.append(
                f"expected at least {len(spec)} message(s), got {len(self.messages)}"
            )
        else:
            actual = self.messages[-len(spec) :] if spec else []
            problems = self._compare(spec, actual, kind="tail")
        if problems:
            raise ContextAssertionError(self._failure("tail mismatch", problems))

    def _compare(
        self,
        spec: Sequence[MsgSpec],
        actual: Sequence[MessageView],
        *,
        kind: str,
    ) -> list[str]:
        problems: list[str] = []
        for position, expected in enumerate(spec):
            if position >= len(actual):
                problems.append(f"{kind}[{position}]: missing (message not present)")
                continue
            message = actual[position]
            reason = match_message(message, expected)
            if reason is not None:
                problems.append(
                    f"message[{message.index}] ({kind} position {position}): {reason}"
                )
        return problems


__all__ = [
    "SPEC_KEYS",
    "SYSTEM_ROLES",
    "ContextAssertionError",
    "ContextView",
    "MessageView",
    "MsgSpec",
    "ToolCallView",
    "flatten_content",
    "match_message",
    "normalize_text",
    "truncate",
]
