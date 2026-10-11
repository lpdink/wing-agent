# wing/tools/experimental/edit.py
"""HashlineEdit——行锚补丁编辑（experimental 命名域）。

注册名 ``Edit``、命名域 ``experimental``（引用 `experimental.Edit`）、LLM 可见名
``HashlineEdit``——与主 ``Edit`` 工具（str_replace 方言）并存、互不撞名。默认不在
任何 agent 的工具集里；会话经 ``agents[].tools`` / ``session/update`` 显式启用。

格式与语义见函数 docstring；核心逻辑在 ``hashline.py``（解析 / 应用 / 窗口）。
命名寄存器是模块级会话状态（``session_id → {name: lines}``，进程内、不落盘），
与 TodoWrite 的会话态同型。
"""

from __future__ import annotations

import os

from wing.agent import ToolContext, current_tool_call_id
from wing.common.fs import atomic_write_text
from wing.event import DiffContentEvent
from wing.schema import ToolError, ToolParam
from wing.tool_registry import tool_registry
from wing.tools.experimental.hashline import (
    ApplyOutcome,
    HashlineError,
    Section,
    apply_section,
    content_tag,
    decode_utf8,
    parse_payload,
)
from wing.tools.internal.utils import resolve_path
from wing.tools.internal.write_target import resolve_write_target

_PARAMS = [
    ToolParam(
        name="input",
        type="string",
        description=(
            "Hashline patch text: one or more [path#TAG] sections of PUT/CUT/"
            "REM/MV ops. See the tool description for the exact format."
        ),
    ),
]

#: 会话级命名寄存器（session_id → {name → lines}）。仅本工具的 CUT / 粘贴读写。
_registers: dict[str, dict[str, list[str]]] = {}


@tool_registry.register(
    name="Edit",
    namespace="experimental",
    llm_name="HashlineEdit",
    params=_PARAMS,
)
async def hashline_edit(input: str, ctx: ToolContext) -> str:
    """Apply a hashline patch: one or more `[path#TAG]` sections of line-anchored ops.

    TAG is the 4-hex tag from a HashlineRead of that file; the patch is rejected
    when the file changed since that read. All line numbers refer to that read's
    snapshot, independent of earlier ops in the same payload. Use Write for new
    files. The `*** Begin Patch` / `*** End Patch` wrapper is optional.

    Ops (per file, any order; positions below are original line numbers):
    - PUT N.=M:   replace lines N–M with the following `+` body lines (N.=N = one line).
    - PUT <N:     insert body before line N; PUT >N: after line N; PUT >$: append at EOF.
    - CUT N.=M    delete lines and capture them; `CUT N.=M @name` keeps a session register.
    - PUT <N @name / PUT >N @name / PUT >$ @name — paste register lines at a gap
      (omit @name to paste the last CUT of this same payload);
      PUT N.=M @name pastes register lines over N–M (a named register is required).
    - REM         delete the whole file (must be the only op in its section).
    - MV DEST     move/rename the file after its edits (may share the section).

    Body lines start with `+`; the rest is written verbatim (a lone `+` is a blank
    line). Content whose own line starts with `-`/`+` must be escaped as `+-`/`++`.

    Example:
    *** Begin Patch
    [greet.py#A1B2]
    PUT 1.=1:
    +def greet(name):
    *** End Patch

    Returns per-file new TAGs (reuse them for follow-up patches). On any error
    nothing is written and the message starts with "hashline: ".
    """
    try:
        sections = parse_payload(input)
    except HashlineError as e:
        raise ToolError(f"hashline: {e}") from e

    session_registers = _registers.setdefault(ctx.session_id, {})

    # 先全部校验（存在性 / tag 对账 / 应用可行性），后统一落盘——任何一段出错
    # 都不产生半截写入。
    prepared: list[tuple[Section, str, str, bool, ApplyOutcome]] = []
    for section in sections:
        resolved = resolve_path(section.path, ctx)
        try:
            with open(resolved, "rb") as f:
                raw = f.read()
        except FileNotFoundError:
            raise ToolError(
                f"hashline: {section.path}: No such file (use Write for new files)"
            )
        except IsADirectoryError:
            raise ToolError(f"hashline: {section.path}: Is a directory")
        except PermissionError:
            raise ToolError(f"hashline: {section.path}: Permission denied")
        except OSError as e:
            raise ToolError(f"hashline: {section.path}: {e.strerror or e}")

        try:
            content, had_bom = decode_utf8(raw)
        except UnicodeDecodeError:
            raise ToolError(f"hashline: {section.path}: not UTF-8 — cannot patch")

        if content_tag(content) != section.tag:
            raise ToolError(
                f"hashline: {section.path}: stale tag #{section.tag}: "
                "the file changed since it was read; re-read it and retry"
            )

        try:
            outcome = apply_section(section, content, session_registers)
        except HashlineError as e:
            raise ToolError(f"hashline: {e}") from e
        prepared.append((section, resolved, content, had_bom, outcome))

    for section, resolved, old_content, had_bom, outcome in prepared:
        try:
            if outcome.removed:
                os.unlink(resolved)
                continue
            text = ("\ufeff" if had_bom else "") + outcome.new_content
            if outcome.moved_to is not None:
                dest = resolve_path(outcome.moved_to, ctx)
                _write(dest, text)
                if os.path.abspath(dest) != os.path.abspath(resolved):
                    os.unlink(resolved)
                continue
            if outcome.new_content != old_content:
                _write(resolved, text)
        except OSError as e:
            raise ToolError(
                f"hashline: {section.path}: write failed: {e.strerror or e}"
            ) from e

    # 寄存器只在全部落盘成功后提交（"published only after their writes land"）。
    for _section, _resolved, _old, _bom, outcome in prepared:
        if outcome.named_registers:
            session_registers.update(outcome.named_registers)

    tool_call_id = current_tool_call_id() or ""
    for _section, resolved, _old, _bom, outcome in prepared:
        if outcome.removed:
            continue
        for window in outcome.windows:
            ctx.emit(
                DiffContentEvent(
                    session_id=ctx.session_id,
                    path=resolved,
                    old_text=window.old_text,
                    new_text=window.new_text,
                    old_start_line=window.old_start_line,
                    new_start_line=window.new_start_line,
                    tool_call_id=tool_call_id,
                )
            )

    parts = [f"hashline: ok ({len(prepared)} file{'s' if len(prepared) != 1 else ''})"]
    for section, _resolved, _old, _bom, outcome in prepared:
        if outcome.removed:
            parts.append(f"  {section.path}: removed")
            continue
        row = f"  {section.path}: #{outcome.new_tag}"
        if outcome.ops_applied:
            row += (
                f", {outcome.ops_applied} op{'s' if outcome.ops_applied != 1 else ''}"
                f", {outcome.old_line_count} → {outcome.new_line_count} lines"
            )
        if outcome.moved_to is not None:
            row += f", moved to {outcome.moved_to}"
        parts.append(row)
    return "\n".join(parts)


def _write(path: str, text: str) -> None:
    """与 Write / Edit 同一份写入政策（符号链接写穿 / 非常规文件就地写）。"""
    target, in_place = resolve_write_target(path)
    if in_place:
        with open(target, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        atomic_write_text(target, text)
