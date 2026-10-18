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
不随会话 fork/恢复复制、网关重启即失忆——与 TodoWrite 的会话态同型。为防长跑
网关无界增长，寄存表有两级水位：跟踪会话数（按首次使用先后 FIFO 淘汰最老条目）
与每会话命名寄存器数（超限显式报错，不静默淘汰）。
"""

from __future__ import annotations

import os
import stat

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
#: 进程内、不落盘；两级水位见模块 docstring。
_registers: dict[str, dict[str, list[str]]] = {}

#: 每会话命名寄存器上限（CUT 可能捕获长文件段；超限显式报错，不静默淘汰）。
_MAX_NAMED_REGISTERS = 64
#: 跟踪的会话数上限（按首次使用先后 FIFO 淘汰最老条目）。
_MAX_TRACKED_SESSIONS = 128

#: prepared 条目：(段, 绝对路径, 原文（未归一化，回滚用）, 归一化文本, BOM, 结果, MV 目标)
_Prepared = tuple[Section, str, str, str, bool, ApplyOutcome, str | None]
#: applied 条目：(绝对路径, 原文, BOM, 结果, MV 目标, 纯重命名的源真实路径, 源权限位)
_Applied = tuple[str, str, bool, ApplyOutcome, str | None, str | None, int | None]


def _session_registers(session_id: str) -> dict[str, list[str]]:
    """取会话寄存器表；表满时按首次使用先后淘汰最老的会话条目。"""
    registers = _registers.get(session_id)
    if registers is not None:
        return registers
    while len(_registers) >= _MAX_TRACKED_SESSIONS:
        _registers.pop(next(iter(_registers)))
    registers = {}
    _registers[session_id] = registers
    return registers


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
    may repeat its section consecutively, later ops are chained onto the same
    snapshot.

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
      refuses an existing destination, the same file, a destination that is
      also patched somewhere in the same payload, or a destination whose
      directory does not exist). A pure move renames the file (the source need
      not be writable), preserving inode, permissions and line endings; a
      moved patch requires a writable source and copies its permissions onto
      the destination.

    Body lines start with `+`; the rest is written verbatim (a lone `+` is a blank
    line). Content whose own line starts with `-`/`+` must be escaped as `+-`/`++`.
    Symlinks and CRLF are handled like the main Edit tool: writes go through the
    link to the real target, and a patch that changes content is written back
    with LF line endings.

    Example:
    *** Begin Patch
    [greet.py#A1B2]
    PUT 1.=1:
    +def greet(name):
    *** End Patch

    Returns per-file new TAGs (reuse them for follow-up patches). A file whose
    normalized content is unchanged is left untouched and reported as
    `no change` (no diff event). On validation errors nothing is written and the
    message starts with "hashline: "; a write failure rolls back the sections
    already written.
    """
    try:
        sections = parse_payload(input)
    except HashlineError as e:
        raise ToolError(f"hashline: {e}") from e

    # 归并键 = realpath：符号链接 / `..` / 相对拼写等别名归一到同一身份。同一文件
    # 的段必须连续——归并后 ops 保持载荷序，寄存器跨段可见性才与 "top-to-bottom
    # across sections" 一致；交错写法（a…b…a）会让中间段看到归并后的最终寄存器，
    # 与声明语义分叉，因此显式报错而不是静默按归并序生效。
    groups: dict[str, tuple[str, Section]] = {}
    last_key: str | None = None
    for section in sections:
        resolved = resolve_path(section.path, ctx)
        key = os.path.realpath(resolved)
        if key != last_key and key in groups:
            raise ToolError(
                f"hashline: {section.path}: sections for one file must be contiguous "
                "— merge the ops into one section or move them together"
            )
        last_key = key
        entry = groups.get(key)
        if entry is None:
            groups[key] = (
                resolved,
                Section(
                    path=section.path,
                    tag=section.tag,
                    ops=list(section.ops),
                    line=section.line,
                ),
            )
            continue
        existing = entry[1]
        if existing.tag != section.tag:
            raise ToolError(
                f"hashline: {section.path}: repeated sections carry different tags "
                f"(#{existing.tag} vs #{section.tag}); consolidate them under one tag"
            )
        existing.ops.extend(section.ops)

    session_registers = _session_registers(ctx.session_id)

    # 载荷内前序段捕获的寄存器对后续段可见（"top-to-bottom across sections"），
    # 但只有全部落盘成功后才提交给会话。
    payload_named: dict[str, list[str]] = {}
    anonymous: list[str] | None = None
    prepared: list[_Prepared] = []
    canonical_sources = set(groups)
    planned_dests: dict[str, str] = {}
    for key, (resolved, section) in groups.items():
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

        pending_names = set(payload_named) | set(outcome.named_registers)
        if len(set(session_registers) | pending_names) > _MAX_NAMED_REGISTERS:
            raise ToolError(
                f"hashline: {section.path}: named register limit reached "
                f"({_MAX_NAMED_REGISTERS} per session); reuse an existing name or "
                "omit @name"
            )

        anonymous = outcome.anonymous_register
        payload_named.update(outcome.named_registers)

        dest: str | None = None
        if outcome.moved_to is not None:
            dest = resolve_path(outcome.moved_to, ctx)
            dest_key = os.path.realpath(dest)
            if dest_key == key:
                raise ToolError(
                    f"hashline: {section.path}: MV destination is the same file"
                )
            if dest_key in canonical_sources:
                raise ToolError(
                    f"hashline: {section.path}: MV destination {outcome.moved_to} is "
                    "also patched in this payload — refusing to overwrite it"
                )
            if dest_key in planned_dests:
                raise ToolError(
                    f"hashline: {section.path}: MV destination {outcome.moved_to} is "
                    "already the destination of the section for "
                    f"{planned_dests[dest_key]}"
                )
            if not os.path.isdir(os.path.dirname(dest)):
                raise ToolError(
                    f"hashline: {section.path}: MV destination directory "
                    f"{os.path.dirname(outcome.moved_to) or '.'} does not exist"
                )
            if os.path.lexists(dest):
                raise ToolError(
                    f"hashline: {section.path}: MV destination {outcome.moved_to} "
                    "already exists — refusing to overwrite it"
                )
            planned_dests[dest_key] = section.path
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
                mode = _unlink_content_target(resolved)
                applied.append((resolved, raw_text, had_bom, outcome, None, None, mode))
                continue
            if dest is not None:
                source = os.path.realpath(resolved)
                if outcome.new_content == norm_text:
                    # 纯移动走 rename：保留 inode / 权限位 / 行尾字节，且只需目录
                    # 写权限（源自身只读也能移动——rename 不写文件）。
                    os.rename(source, dest)
                    applied.append(
                        (resolved, raw_text, had_bom, outcome, dest, source, None)
                    )
                    continue
                # 带内容变更的移动 = 编辑 + 删除源：源须可写（与主 Edit 同一政策）
                resolve_write_target(resolved)
                mode = stat.S_IMODE(os.stat(source).st_mode)
                _write(dest, text)
                applied.append((resolved, raw_text, had_bom, outcome, dest, None, mode))
                try:
                    os.chmod(dest, mode)
                except OSError:
                    pass  # 权限位复制尽力而为；内容已就位，不因此让 MV 失败
                os.unlink(source)
                continue
            if outcome.new_content != norm_text:
                _write(resolved, text)
            else:
                continue
            applied.append((resolved, raw_text, had_bom, outcome, dest, None, None))
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
    for _section, resolved, _raw, norm_text, _bom, outcome, _dest in prepared:
        if outcome.removed or outcome.new_content == norm_text:
            continue  # 无净变化：不写回也不发事件，避免前端渲染空 diff
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
    for section, _resolved, _raw, norm_text, _bom, outcome, _dest in prepared:
        if outcome.removed:
            parts.append(f"  {section.path}: removed")
            continue
        row = f"  {section.path}: #{outcome.new_tag}"
        if outcome.ops_applied:
            row += (
                f", {outcome.ops_applied} op{'s' if outcome.ops_applied != 1 else ''}"
            )
            if outcome.new_content != norm_text:
                row += f", {outcome.old_line_count} → {outcome.new_line_count} lines"
            else:
                row += ", no change"
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


def _unlink_content_target(path: str) -> int:
    """按写穿政策删除内容目标，返回其权限位（回滚重建时还原）。

    链接本身保留（可能悬空）——本工具编辑的是"路径处的内容"，不是目录项。
    只读文件按 ``resolve_write_target`` 的政策拒绝（REM 是销毁性操作）。
    """
    target, _in_place = resolve_write_target(path)
    mode = stat.S_IMODE(os.stat(target).st_mode)
    os.unlink(target)
    return mode


def _rollback(applied: list[_Applied]) -> list[str]:
    """尽力回滚已落盘段——返回失败清单（空 = 完整回滚）。

    恢复写原文（未归一化）；MV 移除已写出的目标，纯重命名改名回源；被删除重建的
    段落同时还原源权限位（原子写对新建文件用默认模式，不还原会静默降级）。任何
    失败只记录、不上抛（原始写失败才是主错误）。
    """
    failures: list[str] = []
    for resolved, raw_text, had_bom, _outcome, dest, renamed_from, mode in reversed(
        applied
    ):
        try:
            if renamed_from is not None and dest is not None:
                os.rename(dest, renamed_from)
                continue
            _write(resolved, ("\ufeff" if had_bom else "") + raw_text)
            if mode is not None:
                os.chmod(resolved, mode)
        except Exception as e:  # noqa: BLE001 — 回滚是尽力而为，失败照实上报
            failures.append(f"{resolved} ({e})")
            continue
        if dest is not None:
            try:
                if os.path.lexists(dest):
                    os.unlink(dest)
            except Exception as e:  # noqa: BLE001
                failures.append(f"{dest} ({e})")
    return failures
