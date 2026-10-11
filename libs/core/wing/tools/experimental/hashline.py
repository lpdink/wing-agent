# wing/tools/experimental/hashline.py
"""Hashline 补丁核心——tag / 解析 / 应用 / diff 窗口（纯逻辑，无 IO）。

格式借鉴 oh-my-pi 的 hashline 编辑格式（``crates/pi-edit/grammars/hashline.lark``），
服务于 ``experimental`` 命名域的 HashlineRead / HashlineEdit 一组实验工具：

- 载荷是一串 ``[PATH#TAG]`` 文件段，每段跟着若干行锚操作；
- ``TAG`` 是内容指纹（``content_tag``），由 HashlineRead 随内容一起给出；应用前
  与磁盘现状对账——对不上即拒绝（不按过期认知写文件）；
- 行号一律相对 read 时的原文快照：同一次载荷里多个操作的定位互不影响。

v1 不支持块定位（``N*`` / ``>N*``）——上游靠 tree-sitter 解析语法块，本实验没有
语法解析依赖，遇到块形式显式报错，改用显式行范围。
"""

from __future__ import annotations

import hashlib
import re
from collections.abc import Mapping
from dataclasses import dataclass, field

#: 段头 ``[path#TAG]``；路径不含 ``#``（与上游同约束），TAG 为 4 位十六进制。
_SECTION_RE = re.compile(r"^\[(?P<path>[^#\r\n]+)#(?P<tag>[0-9A-Fa-f]{4})\]$")
_RANGE_RE = re.compile(r"^(?P<start>[1-9]\d*)\.=(?P<end>[1-9]\d*)$")
_BEFORE_RE = re.compile(r"^<(?P<line>[1-9]\d*)$")
_AFTER_RE = re.compile(r"^>(?P<line>[1-9]\d*)$")
_BLOCK_RE = re.compile(r"^(?:>[1-9]\d*|[1-9]\d*)\*$")
_REGISTER_RE = re.compile(r"\s+@(?P<name>[A-Za-z0-9_-]+)$")

_BEGIN_WRAP = "*** Begin Patch"
_END_WRAP = "*** End Patch"
_MV_PREFIX = "MV "

#: diff 窗口在变更区间两侧各保留的上下文行数（与工具的 Edit 相同口径）。
DIFF_CONTEXT_LINES = 3


class HashlineError(ValueError):
    """解析 / 应用错误——工具层翻译成 ``ToolError`` 后交给模型。"""


def content_tag(content: str) -> str:
    """内容指纹：LF 规范化后的 sha256 前 4 位十六进制（大写）。

    规范化只折叠换行（``\\r\\n`` / ``\\r`` → ``\\n``），不做空白裁剪——行尾风格
    不属于内容同一性，同一份文本视图给出同一个 tag。
    """
    normalized = content.replace("\r\n", "\n").replace("\r", "\n")
    return hashlib.sha256(normalized.encode("utf-8")).hexdigest()[:4].upper()


def decode_utf8(raw: bytes) -> tuple[str, bool]:
    """字节 →（文本, 是否带 UTF-8 BOM）。

    BOM 不属于内容（读侧不显示、tag 不涵盖），写回时由调用方按标志还原。
    非 UTF-8 抛 ``UnicodeDecodeError``，由工具层翻译。
    """
    if raw.startswith(b"\xef\xbb\xbf"):
        return raw.decode("utf-8-sig"), True
    return raw.decode("utf-8"), False


def split_lines(content: str) -> tuple[list[str], bool]:
    """内容 →（行列表, 是否有收尾换行）。

    与上游 ``str::lines()`` 对齐：``"a\\nb\\n"`` → ``(["a", "b"], True)``；
    空内容 → ``([], False)``。
    """
    if content == "":
        return [], False
    raw = content.split("\n")
    if raw[-1] == "":
        return raw[:-1], True
    return raw, False


# ── 语法树 ────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Loc:
    """操作定位子。kind ∈ {range, before, after, tail}。"""

    kind: str
    start: int = 0
    end: int = 0


@dataclass
class PutOp:
    """PUT 家族：带 body 的替换/插入，或从寄存器粘贴（``body=None``）。"""

    loc: Loc
    body: list[str] | None
    register: str | None
    line: int


@dataclass
class CutOp:
    start: int
    end: int
    register: str | None
    line: int


@dataclass
class RemOp:
    line: int


@dataclass
class MvOp:
    dest: str
    line: int


Op = PutOp | CutOp | RemOp | MvOp


@dataclass
class Section:
    path: str
    tag: str
    ops: list[Op]
    line: int
    """段头在载荷里的 1 起行号（报错定位用）。"""


@dataclass(frozen=True)
class Window:
    """一条 diff 窗口：两侧文本 + 各自首行的 1 起绝对行号。"""

    old_text: str
    new_text: str
    old_start_line: int
    new_start_line: int


@dataclass
class ApplyOutcome:
    path: str
    new_content: str
    new_tag: str
    windows: list[Window]
    ops_applied: int
    """内容操作（PUT / CUT）计数；MV / REM 不计入。"""
    old_line_count: int
    new_line_count: int
    named_registers: dict[str, list[str]] = field(default_factory=dict)
    """本次 CUT 产生的命名寄存器，由调用方在落盘成功后提交。"""
    removed: bool = False
    moved_to: str | None = None


# ── 解析 ──────────────────────────────────────────────────────────────


def parse_payload(text: str) -> list[Section]:
    """解析整份载荷 → 文件段列表。

    两种形态都收：裸段序列，或 ``*** Begin Patch`` / ``*** End Patch`` 包一层
    （上游的严格自定义工具语法要求包装；这里宽松兼容）。段间空行忽略。
    """
    lines = [ln[:-1] if ln.endswith("\r") else ln for ln in text.split("\n")]
    total = len(lines)
    idx = 0

    def skip_blanks() -> None:
        nonlocal idx
        while idx < total and lines[idx].strip() == "":
            idx += 1

    skip_blanks()
    if idx < total and lines[idx].strip() == _BEGIN_WRAP:
        idx += 1

    sections: list[Section] = []
    while True:
        skip_blanks()
        if idx >= total:
            break
        line = lines[idx]
        if line.strip() == _END_WRAP:
            idx += 1
            break
        match = _SECTION_RE.match(line)
        if match is None:
            raise HashlineError(
                f"line {idx + 1}: expected a [path#TAG] section header, got: {line!r}"
            )
        section = Section(
            path=match.group("path"),
            tag=match.group("tag").upper(),
            ops=[],
            line=idx + 1,
        )
        idx += 1
        idx = _parse_ops(lines, idx, section)
        sections.append(section)

    if not sections:
        raise HashlineError("empty patch: expected at least one [path#TAG] section")
    skip_blanks()
    if idx < total:
        raise HashlineError(f"line {idx + 1}: unexpected content after the patch")
    return sections


def _parse_ops(lines: list[str], idx: int, section: Section) -> int:
    total = len(lines)
    while idx < total:
        line = lines[idx]
        if line.strip() == "":
            idx += 1  # 段内空行按噪声忽略（段头/End Patch 之外的自由分隔）
            continue
        if line.strip() == _END_WRAP or _SECTION_RE.match(line):
            break
        line_no = idx + 1
        if line == "REM":
            section.ops.append(RemOp(line=line_no))
            idx += 1
            continue
        if line == "MV" or line.startswith(_MV_PREFIX):
            dest = _unquote(line[len(_MV_PREFIX) :].strip()) if line != "MV" else ""
            if not dest:
                raise HashlineError(f"line {line_no}: MV needs a destination path")
            section.ops.append(MvOp(dest=dest, line=line_no))
            idx += 1
            continue
        if line.startswith("CUT "):
            loc_text, register = _split_register(line[4:].strip())
            token = loc_text.strip()
            if _BLOCK_RE.match(token):
                raise HashlineError(
                    f"line {line_no}: block locators ({token!r}) are not supported in "
                    "this experimental tool; use an explicit range (N.=M)"
                )
            match = _RANGE_RE.match(token)
            if match is None:
                raise HashlineError(
                    f"line {line_no}: CUT needs an explicit range (N.=M); "
                    f"got: {token!r}"
                )
            start, end = int(match.group("start")), int(match.group("end"))
            if end < start:
                raise HashlineError(
                    f"line {line_no}: invalid range {token!r} (end < start)"
                )
            section.ops.append(
                CutOp(start=start, end=end, register=register, line=line_no)
            )
            idx += 1
            continue
        if line.startswith("PUT "):
            rest = line[4:]
            if rest.endswith(":"):
                loc = _parse_locator(rest[:-1], line_no)
                body, idx = _parse_body(lines, idx + 1, line_no)
                section.ops.append(
                    PutOp(loc=loc, body=body, register=None, line=line_no)
                )
                continue
            loc_text, register = _split_register(rest)
            loc = _parse_locator(loc_text, line_no)
            section.ops.append(
                PutOp(loc=loc, body=None, register=register, line=line_no)
            )
            idx += 1
            continue
        if line.startswith("+"):
            raise HashlineError(
                f"line {line_no}: body line without a preceding 'PUT ...:' header"
            )
        raise HashlineError(
            f"line {line_no}: expected an op (PUT/CUT/REM/MV), a section header, "
            f"or {_END_WRAP!r}; got: {line!r}"
        )
    return idx


def _parse_body(lines: list[str], idx: int, op_line: int) -> tuple[list[str], int]:
    body: list[str] = []
    total = len(lines)
    while idx < total and lines[idx].startswith("+"):
        body.append(lines[idx][1:])
        idx += 1
    if not body:
        raise HashlineError(
            f"line {op_line}: 'PUT ...:' needs at least one following '+' body line"
        )
    return body, idx


def _split_register(tail: str) -> tuple[str, str | None]:
    """拆出尾部的 `` @name``；无寄存器时原样返回（调用方 trim）。"""
    match = _REGISTER_RE.search(tail)
    if match is None:
        return tail, None
    return tail[: match.start()].rstrip(), match.group("name")


def _parse_locator(text: str, line_no: int) -> Loc:
    token = text.strip()
    if _BLOCK_RE.match(token):
        raise HashlineError(
            f"line {line_no}: block locators ({token!r}) are not supported in this "
            "experimental tool; use an explicit range (N.=M)"
        )
    match = _RANGE_RE.match(token)
    if match:
        start, end = int(match.group("start")), int(match.group("end"))
        if end < start:
            raise HashlineError(
                f"line {line_no}: invalid range {token!r} (end < start)"
            )
        return Loc(kind="range", start=start, end=end)
    match = _BEFORE_RE.match(token)
    if match:
        return Loc(kind="before", start=int(match.group("line")))
    match = _AFTER_RE.match(token)
    if match:
        return Loc(kind="after", start=int(match.group("line")))
    if token == ">$":
        return Loc(kind="tail")
    raise HashlineError(f"line {line_no}: invalid locator {token!r}")


def _unquote(text: str) -> str:
    if len(text) >= 2 and text[0] == text[-1] and text[0] in ("'", '"'):
        return text[1:-1]
    return text


# ── 应用 ──────────────────────────────────────────────────────────────


def apply_section(
    section: Section,
    content: str,
    registers: Mapping[str, list[str]],
) -> ApplyOutcome:
    """把一段补丁应用到 ``content``（调用方已通过 tag 对账）。

    先做全量校验与内容构造，任何错误（越界 / 重叠 / 未知寄存器）都抛
    ``HashlineError`` 且不产生半截结果；写盘全部在调用方。
    """
    lines, trailing_nl = split_lines(content)
    total = len(lines)
    ops = section.ops

    if any(isinstance(op, RemOp) for op in ops) and len(ops) != 1:
        rem = next(op for op in ops if isinstance(op, RemOp))
        raise HashlineError(f"line {rem.line}: REM must be the only op in its section")

    # 定位 → (排序键, 类型, 载荷, 行号)。排序键 = (位置, 插入优先, 出现序)：
    # 同一位置上"插入"先于"替换"（插到被替换区间的头顶）。
    primitives: list[tuple[tuple[int, int, int], str, tuple, int]] = []
    seq = 0
    named: dict[str, list[str]] = {}
    anonymous: list[str] | None = None
    removed = False
    moved_to: str | None = None
    ops_applied = 0

    def add_insert(boundary: int, body: list[str], line_no: int) -> None:
        nonlocal seq
        if not 1 <= boundary <= total + 1:
            raise HashlineError(
                f"line {line_no}: insert position out of range (file has {total} lines)"
            )
        primitives.append(((boundary, 0, seq), "insert", (boundary, body), line_no))
        seq += 1

    def add_replace(start: int, end: int, body: list[str], line_no: int) -> None:
        nonlocal seq
        if not 1 <= start <= end <= total:
            raise HashlineError(
                f"line {line_no}: range {start}.={end} out of range "
                f"(file has {total} lines)"
            )
        primitives.append(((start, 1, seq), "replace", (start, end, body), line_no))
        seq += 1

    def resolve_register(register: str | None, line_no: int) -> list[str]:
        if register is None:
            if anonymous is None:
                raise HashlineError(
                    f"line {line_no}: no anonymous register — CUT without @name "
                    "earlier in this payload first"
                )
            return list(anonymous)
        if register in named:
            return list(named[register])
        if register in registers:
            return list(registers[register])
        raise HashlineError(f"line {line_no}: unknown register @{register}")

    def gap_boundary(loc: Loc) -> int:
        if loc.kind == "before":
            return loc.start
        if loc.kind == "after":
            return loc.start + 1
        return total + 1  # tail

    for op in ops:
        if isinstance(op, RemOp):
            removed = True
            continue
        if isinstance(op, MvOp):
            moved_to = op.dest
            continue
        if isinstance(op, CutOp):
            if not 1 <= op.start <= op.end <= total:
                raise HashlineError(
                    f"line {op.line}: range {op.start}.={op.end} out of range "
                    f"(file has {total} lines)"
                )
            captured = list(lines[op.start - 1 : op.end])
            anonymous = captured
            if op.register is not None:
                named[op.register] = captured
            add_replace(op.start, op.end, [], op.line)
            ops_applied += 1
            continue
        # PutOp
        if op.body is None:
            body = resolve_register(op.register, op.line)
            if op.loc.kind == "range":
                add_replace(op.loc.start, op.loc.end, body, op.line)
            else:
                add_insert(gap_boundary(op.loc), body, op.line)
        elif op.loc.kind == "range":
            add_replace(op.loc.start, op.loc.end, op.body, op.line)
        else:
            add_insert(gap_boundary(op.loc), op.body, op.line)
        ops_applied += 1

    # 重叠校验：替换区间互不相交；插入点不得落在替换区间内部。
    replaces = sorted(p for p in primitives if p[1] == "replace")
    for prev, cur in zip(replaces, replaces[1:]):
        if cur[2][0] <= prev[2][1]:
            raise HashlineError(
                f"line {cur[3]}: overlapping edits "
                f"({prev[2][0]}.={prev[2][1]} and {cur[2][0]}.={cur[2][1]})"
            )
    for prim in primitives:
        if prim[1] != "insert":
            continue
        boundary = prim[2][0]
        for rep in replaces:
            if rep[2][0] < boundary <= rep[2][1]:
                raise HashlineError(
                    f"line {prim[3]}: insert position {boundary} falls inside the "
                    f"replaced range {rep[2][0]}.={rep[2][1]}"
                )

    # 依序构造新内容；同时记录每步的（原文行区间, 新文行区间——构造序保证新侧
    # 位置即终稿位置，后续操作只在其后追加）。
    out: list[str] = []
    spans: list[tuple[tuple[int, int], tuple[int, int]]] = []
    cursor = 1
    for _key, kind, payload, line_no in sorted(primitives, key=lambda p: p[0]):
        if kind == "insert":
            boundary, body = payload
            if cursor > boundary:
                raise HashlineError(f"line {line_no}: insert overlaps an earlier edit")
            out.extend(lines[cursor - 1 : boundary - 1])
            new_start = len(out) + 1
            out.extend(body)
            spans.append(((boundary, boundary - 1), (new_start, len(out))))
            cursor = boundary
        else:
            start, end, body = payload
            if cursor > start:
                raise HashlineError(f"line {line_no}: edit overlaps an earlier edit")
            out.extend(lines[cursor - 1 : start - 1])
            new_start = len(out) + 1
            out.extend(body)
            spans.append(((start, end), (new_start, len(out))))
            cursor = end + 1
    out.extend(lines[cursor - 1 :])

    new_lines = out
    if not new_lines:
        new_content = ""
    else:
        new_content = "\n".join(new_lines) + ("\n" if trailing_nl else "")

    return ApplyOutcome(
        path=section.path,
        new_content=new_content,
        new_tag=content_tag(new_content),
        windows=[_window(lines, new_lines, old, new) for old, new in spans],
        ops_applied=ops_applied,
        old_line_count=total,
        new_line_count=len(new_lines),
        named_registers=named,
        removed=removed,
        moved_to=moved_to,
    )


def _window(
    old_lines: list[str],
    new_lines: list[str],
    old_span: tuple[int, int],
    new_span: tuple[int, int],
    context: int = DIFF_CONTEXT_LINES,
) -> Window:
    """行区间 → diff 窗口：两侧各向上下游扩 ``context`` 行并裁剪。

    零长度区间（插入 / 纯删除的另一侧）按"间隙"表达：``(b, b-1)`` 表示第 b 行
    之前的间隙，窗口取 ``b-3 .. b+2``。
    """

    def clip(first: int, last: int, total: int) -> tuple[int, int, int]:
        lo = max(1, first - context)
        hi = min(total, last + context)
        if hi < lo:
            return 1, 0, 1
        return lo, hi, lo

    old_lo, old_hi, old_start = clip(old_span[0], old_span[1], len(old_lines))
    new_lo, new_hi, new_start = clip(new_span[0], new_span[1], len(new_lines))
    return Window(
        old_text="\n".join(old_lines[old_lo - 1 : old_hi]),
        new_text="\n".join(new_lines[new_lo - 1 : new_hi]),
        old_start_line=old_start,
        new_start_line=new_start,
    )
