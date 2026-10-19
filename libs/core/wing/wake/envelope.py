"""wing/wake/envelope.py — 内置信封渲染（纯函数）。

投递到 target 的是一条**普通用户消息**：目标会话照常把它当输入处理（开着 TUI 的会话会把它渲染成
用户消息，因为投递带了 ``request_id``）。信封只负责「一眼看懂发生了什么 + 下一条命令怎么拼」。

三种形态（契约见 ``docs/dev/wake.md``）：

- done：source 收口（任意 subtype）；
- ask：source 提问（带 ``tool_call_id``，回答必须定向回那一个 waiter）；
- timer：定时唤醒（``prompt`` 原文 + 一行出处）。

用户的 ``--prompt`` 对 session 型触发器是**原样发送**（不做任何模板替换）——这里因此返回
``str | None``：``None`` = 「该发 prompt 原文」，由调用方决定。
"""

from __future__ import annotations

from typing import cast

from wing.wake.model import WakeTrigger

#: 最后一条 assistant 正文的截断长度（字符）。信封是通知，不是转储。
MESSAGE_LIMIT = 2000

_EMPTY_TEXT = "(no text)"


def truncate(text: str, limit: int = MESSAGE_LIMIT) -> str:
    """截断到 ``limit`` 字符（截断时补一个省略号，长度因此是 ``limit + 1``）。"""
    body = text or ""
    if len(body) <= limit:
        return body
    return body[:limit] + "…"


def format_seconds(duration_ms: int | None) -> str:
    """毫秒 → 信封里的秒数（一位小数；缺省当 0）。"""
    return f"{(int(duration_ms or 0) / 1000):.1f}"


def done_envelope(
    *,
    session_id: str,
    subtype: str,
    num_turns: int,
    duration_ms: int,
    result: str | None,
) -> str:
    """source 收口信封。"""
    body = truncate(result.strip()) if result and result.strip() else _EMPTY_TEXT
    return (
        f"[wing] session {session_id} finished "
        f"(subtype={subtype}, turns={num_turns}, {format_seconds(duration_ms)}s)\n"
        f"\n"
        f"Last message:\n"
        f"{body}\n"
        f"\n"
        f"Inspect:   wing tail {session_id}\n"
        f'Continue:  wing run -r {session_id} -p "..."'
    )


def ask_envelope(
    *,
    session_id: str,
    tool_call_id: str,
    questions: list[dict] | None = None,
    question: str = "",
    choices: list[str] | None = None,
) -> str:
    """source 提问信封（多问题与 legacy 单问题两种载荷）。

    legacy（Bash 危险命令确认）只有 ``question`` + ``choices``，没有 header：那一行退化成
    ``- <question>  (y / n / yolo)``。
    """
    lines: list[str] = []
    for item in questions or []:
        header = str(item.get("header") or "").strip()
        text = str(item.get("question") or "").strip()
        if not text:
            continue
        prefix = f"[{header}] " if header else ""
        lines.append(f"- {prefix}{text}{_options(item.get('options'))}")
    if not lines and question.strip():
        lines.append(f"- {question.strip()}{_choices(choices)}")
    if not lines:
        lines.append(f"- {_EMPTY_TEXT}")

    answer_hint = (
        f'Answer: wing run -r {session_id} --tool-call-id {tool_call_id} -p "<answer>"'
    )
    return (
        f"[wing] session {session_id} is waiting for your answer "
        f"(tool_call_id={tool_call_id})\n"
        f"\n"
        f"Question:\n" + "\n".join(lines) + f"\n\n{answer_hint}"
    )


def timer_envelope(*, spec: str, ordinal: int, prompt: str) -> str:
    """定时唤醒信封（``#<n>`` = 本次是第几次触发）。"""
    return f"[wing] scheduled wake ({spec}, #{ordinal})\n\n{prompt}"


def session_payload(trigger: WakeTrigger, envelope: str) -> str:
    """session 型触发器的实际投递内容：用户给了 ``prompt`` 就**原样发送**，否则用信封。"""
    return trigger.prompt if trigger.prompt else envelope


def _options(options: object) -> str:
    """``options: [{label, description}]`` → ``  (a / b)``（无选项时为空串）。"""
    if not isinstance(options, list):
        return ""
    labels = [_field(item, "label") for item in options]
    labels = [label for label in labels if label]
    return f"  ({' / '.join(labels)})" if labels else ""


def _field(item: object, key: str) -> str:
    """wire 载荷里的一个字符串字段（非映射 / 缺失 / 空 → 空串）。"""
    if not isinstance(item, dict):
        return ""
    entry = cast("dict[str, object]", item)
    return str(entry.get(key) or "").strip()


def _choices(choices: list[str] | None) -> str:
    labels = [str(c).strip() for c in (choices or []) if str(c).strip()]
    return f"  ({' / '.join(labels)})" if labels else ""
