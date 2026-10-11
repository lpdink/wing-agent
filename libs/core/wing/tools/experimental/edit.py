# wing/tools/experimental/edit.py
"""HashlineEdit——行锚补丁编辑（experimental 命名域）。

注册名 ``Edit``、命名域 ``experimental``（引用 `experimental.Edit`）、LLM 可见名
``HashlineEdit``——与主 ``Edit`` 工具（str_replace 方言）并存、互不撞名。默认不在
任何 agent 的工具集里；会话经 ``agents[].tools`` / ``session/update`` 显式启用。

格式与语义见函数 docstring；核心逻辑在 ``hashline.py``。两条实现约束：

- **先全量校验、后统一落盘**：校验（存在性 / tag 对账 / 应用可行性 / MV 目标）
  任何一段失败 → 零写入；落盘中途失败对已写段做尽力回滚（回滚失败照实上报）。
- **同一路径的重复段按载荷顺序归并**（同一快照、ops 拼接）：不归并的话，后段的
  完整文件由原始快照推导，会覆盖前段落盘结果——前段编辑静默丢失。

命名寄存器是模块级会话状态（``session_id → {name: lines}``）：进程内、不落盘、
不随会话 fork/恢复复制、网关重启即失忆——与 TodoWrite 的会话态同型。
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
    normalize_newlines,
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

#: 会话级命名寄存器（session_id → {name → lines}）。仅本工具的 CUT / 粘贴读写；
#: 进程内、不落盘（见模块 docstring 的作用域说明）。
_registers: dict[str, dict[str, list[str]]] = {}

#: prepared 条目：(段, 绝对路径, 原文（未归一化，回滚用）, 归一化文本, BOM, 结果, MV 目标)
_Prepared = tuple[Section, str, str, str, bool, ApplyOutcome, str | None]
#: applied 条目：(绝对路径, 原文, BOM, 结果, MV 目标)
_Applied = tuple[str, str, bool, ApplyOutcome, str | None]


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
    files. The `*** Begin Patch` / `*** End Patch` wrapper is optional; a file
    may repeat its section, later ops are chained onto the same snapshot.

    Ops (per file, any order; positions below are original line numbers):
    - PUT N.=M:   replace lines N–M with the following `+` body lines (N.=N = one line).
    - PUT <N:     insert body before line N; PUT >N: after line N; PUT >$: append at EOF.
    - CUT N.=M    delete lines and capture them; `CUT N.=M @name` keeps a named
      register for the rest of the session (in-memory; a gateway restart drops it).
    - PUT <N @name / PUT >N @name / PUT >$ @name — paste register lines at a gap
      (omit @name to paste the last CUT of this same payload);
      PUT N.=M @name pastes register lines over N–M (@name required).
    - REM         delete the whole file (must be the only op in its section).
    - MV DEST     move/rename the file after its edits (may share the section;
      refuses an existing destination).

    Body lines start with `+`; the rest is written verbatim (a lone `+` is a blank
    line). Content whose own line starts with `-`/`+` must be escaped as `+-`/`++`.
    Symlinks and CRLF are handled like the main Edit tool: writes go through the
    link to the real target, and the file is rewritten with LF line endings.

    Example:
    *** Begin Patch
    [greet.py#A1B2]
    PUT 1.=1:
    +def greet(name):
    *** End Patch

    Returns per-file new TAGs (reuse them for follow-up patches). On validation
    errors nothing is written and the message starts with "hashline: "; a write
    failure rolls back the sections already written.
    """
    try:
        sections = parse_payload(input)
    except HashlineError as e:
        raise ToolError(f"hashline: {e}") from e

    merged: dict[str, Section] = {}
    for section in sections:
        resolved = resolve_path(section.path, ctx)
        existing = merged.get(resolved)
        if existing is None:
            merged[resolved] = Section(
                path=section.path,
                tag=section.tag,
                ops=list(section.ops),
                line=section.line,
            )
            continue
        if existing.tag != section.tag:
            raise ToolError(
                f"hashline: {section.path}: repeated sections carry different tags "
                f"(#{existing.tag} vs #{section.tag}); consolidate them under one tag"
            )
        existing.ops.extend(section.ops)

    session_registers = _registers.setdefault(ctx.session_id, {})

    # 载荷内前序段捕获的寄存器对后续段可见（"top-to-bottom across sections"），
    # 但只有全部落盘成功后才提交给会话。
    payload_named: dict[str, list[str]] = {}
    anonymous: list[str] | None = None
    prepared: list[_Prepared] = []
    for resolved, section in merged.items():
        try:
            with open(resolved, "rb") as f:
                raw = f.read()
        except FileNotFoundError:
            raise ToolError(
                f"hashline: {section.path}: No such file — HashlineEdit patches "
                "existing files only (create new files with Write when available)"
            )
        except IsADirectoryError:
            raise ToolError(f"hashline: {section.path}: Is a directory")
        except PermissionError:
            raise ToolError(f"hashline: {section.path}: Permission denied")
        except OSError as e:
            raise ToolError(f"hashline: {section.path}: {e.strerror or e}")

        try:
            raw_text, had_bom = decode_utf8(raw)
        except UnicodeDecodeError:
            raise ToolError(f"hashline: {section.path}: not UTF-8 — cannot patch")
        norm_text = normalize_newlines(raw_text)

        if content_tag(norm_text) != section.tag:
            raise ToolError(
                f"hashline: {section.path}: stale tag #{section.tag}: "
                "the file changed since it was read; re-read it and retry"
            )

        try:
            outcome = apply_section(
                section, norm_text, {**session_registers, **payload_named}, anonymous
            )
        except HashlineError as e:
            raise ToolError(f"hashline: {e}") from e
        anonymous = outcome.anonymous_register
        payload_named.update(outcome.named_registers)

        dest: str | None = None
        if outcome.moved_to is not None:
            dest = resolve_path(outcome.moved_to, ctx)
            if os.path.abspath(dest) == os.path.abspath(resolved):
                raise ToolError(
                    f"hashline: {section.path}: MV destination is the same file"
                )
            if os.path.lexists(dest):
                raise ToolError(
                    f"hashline: {section.path}: MV destination {outcome.moved_to} "
                    "already exists — refusing to overwrite it"
                )
        prepared.append(
            (section, resolved, raw_text, norm_text, had_bom, outcome, dest)
        )

    applied: list[_Applied] = []
    # except 分支要引用失败段做错误文案（循环变量），预绑定一次。
    section = prepared[0][0]
    try:
        for section, resolved, raw_text, norm_text, had_bom, outcome, dest in prepared:
            text = ("\ufeff" if had_bom else "") + outcome.new_content
            if outcome.removed:
                _unlink_content_target(resolved)
            elif dest is not None:
                _write(dest, text)
                applied.append((resolved, raw_text, had_bom, outcome, dest))
                _unlink_content_target(resolved)
                continue
            elif outcome.new_content != norm_text:
                _write(resolved, text)
            else:
                continue
            applied.append((resolved, raw_text, had_bom, outcome, dest))
    except OSError as e:
        failures = _rollback(applied)
        where = f"hashline: {section.path}: write failed: {e.strerror or e}"
        if failures:
            raise ToolError(
                f"{where}; rollback failed for: {'; '.join(failures)}"
            ) from e
        raise ToolError(f"{where}; nothing was changed") from e

    # 寄存器只在全部落盘成功后提交（"published only after their writes land"）。
    session_registers.update(payload_named)

    tool_call_id = current_tool_call_id() or ""
    for _section, resolved, _raw, _norm, _bom, outcome, _dest in prepared:
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
    for section, _resolved, _raw, _norm, _bom, outcome, _dest in prepared:
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


def _unlink_content_target(path: str) -> None:
    """按写穿政策删除内容目标：符号链接作用于其真实目标（与 Write / Edit 一致）。

    链接本身保留（可能悬空）——本工具编辑的是"路径处的内容"，不是目录项。
    """
    target, _in_place = resolve_write_target(path)
    os.unlink(target)


def _rollback(applied: list[_Applied]) -> list[str]:
    """尽力回滚已落盘段——返回失败清单（空 = 完整回滚）。

    恢复写原文（未归一化），MV 同时移除已写出的目标。任何失败只记录、不上抛
    （原始写失败才是主错误）。
    """
    failures: list[str] = []
    for resolved, raw_text, had_bom, _outcome, dest in reversed(applied):
        try:
            _write(resolved, ("\ufeff" if had_bom else "") + raw_text)
        except Exception as e:  # noqa: BLE001 — 回滚是尽力而为，失败照实上报
            failures.append(f"{resolved} ({e})")
        if dest is not None:
            try:
                if os.path.lexists(dest):
                    os.unlink(dest)
            except Exception as e:  # noqa: BLE001
                failures.append(f"{dest} ({e})")
    return failures
