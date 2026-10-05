"""分层守门 —— AST 解析 ``libs/core/wing/**`` 的 import 关系，锁定目标分层。

规则、分层图与迁移映射见 ``docs/dev/backend-layout.md``。本文件是"可执行的那一份"。

设计要点
--------
- **纯 AST，不 import wing**：解析源码文本而非导入源码，规则判定与包能否成功导入无关。
- 收集每个 ``.py`` 的 ``ast.Import`` / ``ast.ImportFrom``：模块级、``if TYPE_CHECKING:``
  块、函数内懒加载**一律计入**——import 即依赖，懒加载只是延迟代价，不解除依赖。
- 相对 import 按文件所在包绝对化；``from wing import x`` / ``from . import x`` 先用
  **文件系统判定**（不 import）x 是否为子模块——是则记为子模块依赖（``wing.x``），
  否则退回包根（``from wing import execute_shell`` 对应 ``wing/__init__`` re-export 的现状）。
- 每个模块与每个 ``wing.*`` 目标都必须映射到一个"包族"（``FAMILY_RULES``）；
  未登记即失败（强制登记，防止新包/新文件绕过守门）。
- 现状违规锁定在 ``KNOWN_VIOLATIONS``（规则名 -> 条目集合），每条注明由哪个步骤清除。
  判定是**集合严格相等**：
    * 未登记的新违规 -> 红；
    * 已清除/已迁移的条目（stale，含行号漂移）-> 也红——白名单是待办清单，不是豁免开关。
  13 步骤（分层白名单清零）终结时 ``KNOWN_VIOLATIONS`` 必须为空。

规则一览（细节与例外见 docs/dev/backend-layout.md）
---------------------------------------------------
R1 传输隔离：L0–L3 / root 不得 import ``wing.gateway.*``（runtime/background 例外）。
R2 工具不越层：``tools`` 不得 import gateway/runtime/background/session/context/store；
   agent 侧只允许 ``wing.agent`` 包根的**窄接口符号**（ToolContext / current_tool_call_id）
   与 ``wing.agent.tool_context``。
R3 provider 独立：``provider`` 不得 import agent/session/context/runtime/gateway/tools。
R4 存储不反向：``store`` 不得 import L3/L4 任一族。
R5 叶子纯净：``wing/common/**``、``wing/media*``、``wing/schema*`` 不得 import 叶子组
   ``{common, media, schema}`` 之外的 wing 包（规则内例外：3 条**函数体内**懒加载，
   见 ``LEAF_LAZY_EXCEPTIONS``）。
R6 顶层无副作用：``wing/__init__.py`` 不得 import 任何 ``wing.*`` 子模块。

已知边界：动态导入（``importlib.import_module("wing…")`` / ``__import__`` / 运行时拼接的
模块名）不在守门范围——静态 AST 不猜运行时才能定名的形态（口径对齐 probe 门禁
``libs/wing-probe/wing_probe/guard.py`` 的残余风险声明）；与它的分工见
``docs/dev/backend-layout.md`` §4。
"""

from __future__ import annotations

import ast
import functools
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterator

import pytest

# ─────────────────────────────────────────────────────────────────────────────
# 路径与包族表
# ─────────────────────────────────────────────────────────────────────────────

CORE_ROOT = Path(__file__).resolve().parents[1]  # libs/core
WING_ROOT = CORE_ROOT / "wing"

L0, L1, L2, L3, L4 = 0, 1, 2, 3, 4


@dataclass(frozen=True)
class Family:
    """一个包族（分层判定的最小单位）。

    ``layer=None`` 表示包入口（``wing/__init__.py``）这类特殊位置。
    """

    name: str
    layer: int | None


# 包族 → 层。目标分层的完整表格（允许依赖 / 每包职责一句话）见 backend-layout.md。
FAMILIES: dict[str, Family] = {
    "common": Family("common", L0),
    "build_info": Family("build_info", L0),
    "schema": Family("schema", L1),
    "media": Family("media", L1),
    "chain": Family("chain", L1),
    "store": Family("store", L2),
    "event": Family("event", L2),
    "hooks": Family("hooks", L2),
    "request_context": Family("request_context", L2),
    "tool_registry": Family("tool_registry", L2),
    "config": Family("config", L3),
    "context": Family("context", L3),
    "session": Family("session", L3),
    "agent": Family("agent", L3),
    "tools": Family("tools", L3),
    "provider": Family("provider", L3),
    "audit": Family("audit", L3),
    "commands": Family("commands", L3),
    "diagnostics": Family("diagnostics", L3),
    "runtime": Family("runtime", L4),
    "background": Family("background", L4),
    "gateway": Family("gateway", L4),
    "root": Family("root", None),
}

# dotted module 前缀 → 包族；最长前缀优先，顺序不敏感。
# 迁移目标路径（wing/chain.py、wing/context/、wing/session/、wing/audit/ …）现在就登记：
# 迁移完成后同一张表自动生效。新增包必须在此登记（未登记 = 测试失败）。
FAMILY_RULES: tuple[tuple[str, str], ...] = (
    # 现状：tracked_list 仍在 wing/common/ 下（07_split_session_chain 迁往 wing/chain.py）
    ("wing.common.tracked_list", "chain"),
    ("wing.chain", "chain"),
    ("wing.common", "common"),
    ("wing.schema", "schema"),
    ("wing.media", "media"),
    ("wing.store", "store"),
    ("wing.event", "event"),
    ("wing.event_bus", "event"),
    ("wing.hook_registry", "hooks"),
    ("wing.hooks", "hooks"),
    ("wing.request_context", "request_context"),
    ("wing.tool_registry", "tool_registry"),
    ("wing.build_info", "build_info"),
    ("wing._build_info", "build_info"),
    ("wing._version", "build_info"),
    ("wing.config", "config"),
    ("wing.default_config", "config"),
    ("wing.context_manager", "context"),
    ("wing.compactor", "context"),
    ("wing.context", "context"),
    ("wing.session", "session"),
    ("wing.session_manager", "session"),
    ("wing.session_reaper", "session"),
    ("wing.agent_template", "session"),
    # 现状：cancel_watch 仍在 wing/agent/ 下（11_rehome 迁往 wing/diagnostics/）
    ("wing.agent.cancel_watch", "diagnostics"),
    ("wing.diagnostics", "diagnostics"),
    ("wing.agent", "agent"),
    ("wing.tools", "tools"),
    ("wing.provider", "provider"),
    ("wing.metrics_registry", "audit"),
    ("wing.audit", "audit"),
    ("wing.magic_command", "commands"),
    ("wing.commands", "commands"),
    ("wing.runtime", "runtime"),
    ("wing.background", "background"),
    ("wing.gateway", "gateway"),
)

# R5 的叶子组：叶子组内互依允许（现状：media→schema、schema→common.token_counter、
# common.token_counter→schema/media），它们都在依赖图最底层，互依不引入上层耦合。
LEAF_GROUP: frozenset[str] = frozenset({"common", "media", "schema"})

# R5 规则内例外（精确到「源文件 → 目标模块」，且**仅当 import 位于函数体内**时生效）：
# 三条函数内懒加载是设计选择（import 期无结构依赖、无加载副作用），且无重构步骤负责——
# 登记为规则的一部分，而不是"待清理违规"。其它任何跨组依赖照常变红：包括模块级的
# 同类 import（same 文件 → 目标，但位置不满足 → 不豁免），由 `in_function` 维度强制。
# 详情见 docs/dev/backend-layout.md §6 规则内例外。
LEAF_LAZY_EXCEPTIONS: frozenset[tuple[str, str]] = frozenset(
    {
        ("wing/common/logger.py", "wing.config"),
        ("wing/common/with_retry.py", "wing.event"),
        ("wing/common/with_retry.py", "wing.event_bus"),
    }
)


def _family_of(module: str) -> Family | None:
    """dotted module 名 → 包族（最长前缀优先）。未登记返回 None。"""
    if module == "wing":
        return FAMILIES["root"]  # wing/__init__.py（精确匹配，wing.* 不落入 root）
    best: tuple[int, str] | None = None
    for prefix, name in FAMILY_RULES:
        if module == prefix or module.startswith(prefix + "."):
            if best is None or len(prefix) > best[0]:
                best = (len(prefix), name)
    return FAMILIES[best[1]] if best else None


def _is_leaf_path(rel_path: str) -> bool:
    """R5 的适用范围按**当前物理路径**判定（proposal：wing.common.* / wing.media* / wing.schema*）。"""
    return (
        rel_path.startswith("wing/common/")
        or rel_path in ("wing/media.py", "wing/schema.py")
        or rel_path.startswith("wing/media/")
        or rel_path.startswith("wing/schema/")
    )


# ─────────────────────────────────────────────────────────────────────────────
# import 扫描
# ─────────────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class ImportEdge:
    """一条 wing 内部依赖边（源文件里的一个 import 语句）。"""

    src_rel: str  # 相对 libs/core 的路径，如 "wing/session.py"
    lineno: int
    dst_module: str  # 目标模块，如 "wing.gateway.protocol"
    names: tuple[
        str, ...
    ]  # import 的符号名（ImportFrom 的 alias.name / Import 的模块名）——符号级规则用
    in_function: bool  # 是否嵌套在函数体内（懒加载）——R5 例外依赖此维度
    stmt: str  # import 语句源码（单行化，用于失败信息）

    @property
    def key(self) -> str:
        """白名单条目格式：``wing/session.py:36 → wing.gateway.protocol``。"""
        return f"{self.src_rel}:{self.lineno} → {self.dst_module}"


@dataclass(frozen=True)
class ScanResult:
    edges: tuple[ImportEdge, ...]
    problems: tuple[str, ...]  # R0：未登记模块 / 未登记目标 / 语法错误


def _rel_path(path: Path) -> str:
    return f"wing/{path.relative_to(WING_ROOT).as_posix()}"


def _module_name(rel_path: str) -> str:
    parts = list(Path(rel_path).with_suffix("").parts)
    if parts[-1] == "__init__":
        parts = parts[:-1]
    return ".".join(parts)


def _stmt_text(source: str, node: ast.AST) -> str:
    segment = ast.get_source_segment(source, node) or ""
    return " ".join(segment.split())


@dataclass(frozen=True)
class ResolvedImport:
    """一条 import 语句归一化后的目标（一条语句可展开成多条：``from . import a, b``）。"""

    node: ast.Import | ast.ImportFrom
    dst_module: str
    names: tuple[str, ...]
    in_function: bool


def _submodule_exists(parent: str, name: str) -> bool:
    """``parent`` 包目录下是否存在名为 ``name`` 的模块 / 子包（纯文件系统判定，不 import）。

    ``from wing import store`` 与 ``from wing.store import …`` 是等价依赖边（都会真的
    进入 ``sys.modules``）——包属性形态必须解析成子模块，否则 R1/R2/R4 会被静默绕过。
    只有 ``name`` 不是子模块时才退回包根（如 ``from wing import execute_shell``，
    对应 ``wing/__init__`` re-export 的现状）。
    """
    if parent != "wing" and not parent.startswith("wing."):
        return False
    tail = parent[len("wing") :].strip(".")
    base_dir = WING_ROOT / tail.replace(".", "/") if tail else WING_ROOT
    child = base_dir / name
    return child.with_suffix(".py").is_file() or child.is_dir()


def _resolve_from(
    node: ast.ImportFrom, rel_path: str
) -> list[tuple[str, tuple[str, ...]]]:
    """``ImportFrom`` → ``[(目标模块, 符号名), …]``；相对 import 按文件所在包绝对化。

    - ``from a.b import c``              -> ``[("a.b", ("c",))]``（只记模块 + 符号名）
    - ``from wing import store``         -> ``[("wing.store", ("store",))]``（子模块，文件系统判定）
    - ``from wing import execute_shell`` -> ``[("wing", ("execute_shell",))]``（非子模块：包根）
    - ``from . import x, y``             -> ``[("<包>.x", ("x",)), …]``（module=None 时逐个判定）
    """
    if node.level == 0:
        if node.module is None:
            return []
        if node.module == "wing":
            return [
                (
                    f"wing.{alias.name}"
                    if _submodule_exists("wing", alias.name)
                    else "wing",
                    (alias.name,),
                )
                for alias in node.names
            ]
        return [(node.module, tuple(alias.name for alias in node.names))]
    # 文件所在包：foo/bar.py -> ["foo"]；foo/__init__.py -> ["foo"]
    pkg_parts = list(Path(rel_path).with_suffix("").parts)[:-1]
    if node.level > len(pkg_parts):
        return []  # 越出包边界（正常代码不会出现）
    base_parts = pkg_parts[: len(pkg_parts) - (node.level - 1)]
    base = ".".join(base_parts)
    if node.module is None:
        return [
            (
                f"{base}.{alias.name}" if _submodule_exists(base, alias.name) else base,
                (alias.name,),
            )
            for alias in node.names
        ]
    return [
        (
            ".".join(base_parts + node.module.split(".")),
            tuple(alias.name for alias in node.names),
        )
    ]


def _walk_imports(
    node: ast.AST, in_function: bool, rel_path: str
) -> Iterator[ResolvedImport]:
    """深度优先、源码顺序地遍历 import 语句，并跟踪「是否位于函数体内」。"""
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        in_function = True  # 装饰器 / 参数无 import；函数体内的一切都算懒加载
    if isinstance(node, ast.Import):
        for alias in node.names:
            yield ResolvedImport(node, alias.name, (alias.name,), in_function)
        return
    if isinstance(node, ast.ImportFrom):
        for dst, names in _resolve_from(node, rel_path):
            yield ResolvedImport(node, dst, names, in_function)
        return
    for child in ast.iter_child_nodes(node):
        yield from _walk_imports(child, in_function, rel_path)


def _iter_imports(tree: ast.AST, rel_path: str) -> Iterator[ResolvedImport]:
    """遍历全部 import 语句（含 TYPE_CHECKING 块与函数内懒加载）。

    与 ``ast.walk`` 的差别：跟踪 ``in_function``（是否嵌套在函数体内）——
    R5 的懒加载例外只对函数体内的 import 生效。
    """
    yield from _walk_imports(tree, False, rel_path)


@functools.lru_cache(maxsize=1)
def _scan() -> ScanResult:
    """扫描 ``libs/core/wing/**``，返回 wing 内部依赖边与登记问题。"""
    edges: list[ImportEdge] = []
    problems: list[str] = []
    for path in sorted(WING_ROOT.rglob("*.py")):
        if "__pycache__" in path.parts:
            continue
        rel = _rel_path(path)
        source = path.read_text(encoding="utf-8")
        try:
            tree = ast.parse(source, filename=rel)
        except SyntaxError as exc:
            problems.append(f"[R0] {rel}: 语法错误，无法解析（{exc}）")
            continue
        if _family_of(_module_name(rel)) is None:
            problems.append(
                f"[R0] {rel}: 模块未登记分层——请在 test_layering.py 的 FAMILY_RULES 登记，"
                f"并在 docs/dev/backend-layout.md 写明归属"
            )
        for resolved in _iter_imports(tree, rel):
            dst = resolved.dst_module
            if not (dst == "wing" or dst.startswith("wing.")):
                continue  # 非 wing 目标（标准库 / 第三方）
            if _family_of(dst) is None:
                problems.append(
                    f"[R0] {rel}:{resolved.node.lineno} → {dst}: 目标模块未登记"
                    f"（模块不存在或新包未登记 FAMILY_RULES）"
                )
                continue
            edges.append(
                ImportEdge(
                    rel,
                    resolved.node.lineno,
                    dst,
                    resolved.names,
                    resolved.in_function,
                    _stmt_text(source, resolved.node),
                )
            )
    return ScanResult(tuple(edges), tuple(problems))


# ─────────────────────────────────────────────────────────────────────────────
# 规则
# ─────────────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class ViolationContext:
    src_rel: str
    src_family: str
    dst_module: str
    dst_family: str
    dst_layer: int | None
    names: tuple[str, ...]
    in_function: bool


def _r1(ctx: ViolationContext) -> bool:
    """R1 传输隔离：L0–L3 与 root 不得 import wing.gateway.*（runtime/background 例外）。"""
    return ctx.dst_family == "gateway" and ctx.src_family not in {
        "gateway",
        "runtime",
        "background",
    }


# R2 的 agent 侧窄接口：tools 允许从 wing.agent 包根导入的符号（其余符号一律禁止）。
# 包根放行是刻意的——agent/__init__ 是 ToolContext / current_tool_call_id 的公共
# re-export 路径；但 WingAgent / Inbound 这类运行时符号不在此列（它们与
# `from wing.agent.core import WingAgent` 是同一个越层依赖的两种写法）。
AGENT_NARROW_INTERFACE: frozenset[str] = frozenset(
    {"ToolContext", "current_tool_call_id", "tool_context"}
)
"""允许经包属性形态 import 的符号。

``tool_context`` 是子模块名：``wing.agent.__init__`` 的 ``from .tool_context import
ToolContext`` 使 ``wing.agent.tool_context`` 成为包属性，``from wing.agent import
tool_context`` 与 ``import wing.agent.tool_context`` 在运行期等价（两者都放行）。
"""


def _r2(ctx: ViolationContext) -> bool:
    """R2 工具不越层。"""
    if ctx.src_family != "tools":
        return False
    if ctx.dst_family in {
        "gateway",
        "runtime",
        "background",
        "session",
        "context",
        "store",
    }:
        return True
    # agent 侧：包根只放行窄接口符号；其余符号与 wing.agent.* 子模块一律禁止。
    if ctx.dst_module == "wing.agent":
        return any(name not in AGENT_NARROW_INTERFACE for name in ctx.names)
    return (
        ctx.dst_module.startswith("wing.agent.")
        and ctx.dst_module != "wing.agent.tool_context"
    )


def _r3(ctx: ViolationContext) -> bool:
    """R3 provider 独立。"""
    return ctx.src_family == "provider" and ctx.dst_family in {
        "agent",
        "session",
        "context",
        "runtime",
        "gateway",
        "tools",
    }


def _r4(ctx: ViolationContext) -> bool:
    """R4 存储不反向：store 不得 import L3/L4 任一族。"""
    return (
        ctx.src_family == "store" and ctx.dst_layer is not None and ctx.dst_layer >= L3
    )


def _r5(ctx: ViolationContext) -> bool:
    """R5 叶子纯净（规则内例外：仅**函数体内**的懒加载，见 LEAF_LAZY_EXCEPTIONS）。"""
    if not _is_leaf_path(ctx.src_rel):
        return False
    if ctx.dst_family in LEAF_GROUP:
        return False
    if ctx.in_function and (ctx.src_rel, ctx.dst_module) in LEAF_LAZY_EXCEPTIONS:
        return False
    return True


def _r6(ctx: ViolationContext) -> bool:
    """R6 顶层无副作用：wing/__init__.py 不得 import 任何 wing.* 子模块。"""
    return ctx.src_rel == "wing/__init__.py" and (
        ctx.dst_module == "wing" or ctx.dst_module.startswith("wing.")
    )


@dataclass(frozen=True)
class Rule:
    id: str
    title: str
    check: Callable[[ViolationContext], bool]


RULES: tuple[Rule, ...] = (
    Rule("R1", "传输隔离", _r1),
    Rule("R2", "工具不越层", _r2),
    Rule("R3", "provider 独立", _r3),
    Rule("R4", "存储不反向", _r4),
    Rule("R5", "叶子纯净", _r5),
    Rule("R6", "顶层无副作用", _r6),
)

_RULE_BY_ID: dict[str, Rule] = {rule.id: rule for rule in RULES}

FIX_HINT = (
    "修复：按 docs/dev/backend-layout.md 的分层规范调整依赖（不要为了绿灯弱化规则）；"
    "若确属过渡期现状，登记进 KNOWN_VIOLATIONS（须注明由哪个步骤清除）。"
)

# ─────────────────────────────────────────────────────────────────────────────
# 白名单：锁定基线（develop@83751e0）的真实违规；不修现状，逐条注明清除步骤。
# 13 步骤（分层白名单清零）终结时本字典必须为空。
# ─────────────────────────────────────────────────────────────────────────────

KNOWN_VIOLATIONS: dict[str, set[str]] = {
    "R1": {
        # cleared by 07_split_session_chain（AgentOverride 移出 gateway/ 到领域层；10 拆分 protocol 时也必须清）
        "wing/session.py:36 → wing.gateway.protocol",
        # cleared by 07_split_session_chain
        "wing/session_manager.py:44 → wing.gateway.protocol",
    },
    "R2": set(),
    "R5": {
        # cleared by 07_split_session_chain（tracked_list 迁出 wing/common/ → wing/chain.py，不再受 R5 约束）
        "wing/common/tracked_list.py:28 → wing.store.base",
        # cleared by 07_split_session_chain（同上；chain 依赖 event 注册表是设计允许项）
        "wing/common/tracked_list.py:171 → wing.event",
    },
    "R6": set(),
}


# ─────────────────────────────────────────────────────────────────────────────
# 违规收集与报告
# ─────────────────────────────────────────────────────────────────────────────


@functools.lru_cache(maxsize=1)
def _violations_by_rule() -> dict[str, tuple[ImportEdge, ...]]:
    scan = _scan()
    found: dict[str, list[ImportEdge]] = {rule.id: [] for rule in RULES}
    for edge in scan.edges:
        src = _family_of(_module_name(edge.src_rel))
        dst = _family_of(edge.dst_module)
        if src is None or dst is None:
            continue  # 未登记的情况由 test_every_module_and_target_is_registered 负责
        ctx = ViolationContext(
            edge.src_rel,
            src.name,
            edge.dst_module,
            dst.name,
            dst.layer,
            edge.names,
            edge.in_function,
        )
        for rule in RULES:
            if rule.check(ctx):
                found[rule.id].append(edge)
    return {rule_id: tuple(edges) for rule_id, edges in found.items()}


def _stale_note(entry: str, actual_keys: set[str]) -> str:
    """对失效白名单条目给出下一步提示（行号漂移 / 已清除 / 文件已迁走）。"""
    src_part, _, dst = entry.partition(" → ")
    path_str, _, lineno_str = src_part.rpartition(":")
    if not (CORE_ROOT / path_str).exists():
        return "源文件已不存在（已迁移或删除）——请删除该条目"
    candidates: list[tuple[int, str]] = []
    for key in sorted(actual_keys):
        key_src, _, key_dst = key.partition(" → ")
        key_path, _, key_lineno = key_src.rpartition(":")
        if (
            key_path == path_str
            and key_dst == dst
            and key_lineno.isdigit()
            and lineno_str.isdigit()
        ):
            candidates.append((abs(int(key_lineno) - int(lineno_str)), key))
    if candidates:
        return f"疑似行号漂移——请更新为 {min(candidates)[1]}"
    return "违规已清除——请删除该条目"


def _assert_rule_clean(rule_id: str) -> None:
    rule = _RULE_BY_ID[rule_id]
    actual = {edge.key for edge in _violations_by_rule()[rule_id]}
    allowed = KNOWN_VIOLATIONS.get(rule_id, set())
    unlisted = sorted(actual - allowed)
    stale = sorted(allowed - actual)
    if not unlisted and not stale:
        return

    lines = [
        f"[{rule.id}] {rule.title}：{len(unlisted)} 条未登记违规、{len(stale)} 条白名单失效"
    ]
    if unlisted:
        lines.append("")
        lines.append("未登记违规（新增/未锁定的越界 import）：")
        for key in unlisted:
            edge = next(e for e in _violations_by_rule()[rule_id] if e.key == key)
            lines.append(f"  {key}")
            lines.append(f"      {edge.stmt}")
        lines.append(f"  {FIX_HINT}")
    if stale:
        lines.append("")
        lines.append("白名单失效条目（白名单是待办清单，不是豁免开关）：")
        for entry in stale:
            lines.append(f"  {entry}")
            lines.append(f"      {_stale_note(entry, actual)}")
    pytest.fail("\n".join(lines), pytrace=False)


def _rule_hits_for_source(source: str, rel_path: str) -> set[str]:
    """对合成源码片段跑一遍 R1–R6（元测试用；不参与全仓扫描）。"""
    src_family = _family_of(_module_name(rel_path))
    if src_family is None:
        raise AssertionError(f"合成路径未登记分层：{rel_path}")
    hits: set[str] = set()
    for resolved in _iter_imports(ast.parse(source), rel_path):
        dst_family = _family_of(resolved.dst_module)
        if dst_family is None:
            continue
        ctx = ViolationContext(
            rel_path,
            src_family.name,
            resolved.dst_module,
            dst_family.name,
            dst_family.layer,
            resolved.names,
            resolved.in_function,
        )
        for rule in RULES:
            if rule.check(ctx):
                hits.add(rule.id)
    return hits


# ─────────────────────────────────────────────────────────────────────────────
# 测试
# ─────────────────────────────────────────────────────────────────────────────


def test_scanner_covers_sources() -> None:
    """守门自身的健全性：确保扫描器真的看到了代码（防止路径写错导致假绿）。"""
    assert WING_ROOT.is_dir(), f"wing 包目录不存在：{WING_ROOT}"
    scan = _scan()
    assert len(scan.edges) > 50, (
        f"扫描到的依赖边过少（{len(scan.edges)} 条），疑扫描器失效"
    )


def test_every_module_and_target_is_registered() -> None:
    """新模块 / 新包（以及不存在的目标模块）必须登记分层，不允许绕过守门。"""
    problems = _scan().problems
    if problems:
        message = [
            "[R0] 未登记的模块 / 目标（守门无法判定层级）：",
            *(f"  {problem}" for problem in problems),
            "  修复：在 test_layering.py 的 FAMILY_RULES 登记包族，并更新 docs/dev/backend-layout.md。",
        ]
        pytest.fail("\n".join(message), pytrace=False)


def test_whitelist_is_wellformed() -> None:
    """白名单条目格式与规则名必须合法，源文件必须存在（防拼写错误）。"""
    problems: list[str] = []
    for rule_id, entries in KNOWN_VIOLATIONS.items():
        if rule_id not in _RULE_BY_ID:
            problems.append(f"未知规则名：{rule_id}")
        for entry in sorted(entries):
            src_part, sep, dst = entry.partition(" → ")
            path_str, csep, lineno = src_part.rpartition(":")
            if not sep or not csep or not lineno.isdigit():
                problems.append(f"条目格式非法（应为 `路径:行 → 模块`）：{entry}")
                continue
            if not path_str.startswith("wing/") or not dst.startswith("wing"):
                problems.append(f"条目须为 wing 内部路径：{entry}")
            if not (CORE_ROOT / path_str).exists():
                problems.append(f"条目源文件不存在：{entry}")
    if problems:
        pytest.fail(
            "[白名单] 条目不规范：\n"
            + "\n".join(f"  {problem}" for problem in problems),
            pytrace=False,
        )


def test_r1_transport_isolation() -> None:
    """R1：L0–L3 不得 import wing.gateway.*（runtime/background 例外）。"""
    _assert_rule_clean("R1")


def test_r2_tools_stay_leafward() -> None:
    """R2：工具不得 import gateway/runtime/background/session/context/store。"""
    _assert_rule_clean("R2")


def test_r3_provider_is_independent() -> None:
    """R3：provider 不依赖 agent/session/context/runtime/gateway/tools。"""
    _assert_rule_clean("R3")


def test_r4_store_has_no_upward_deps() -> None:
    """R4：store 不依赖 L3/L4。"""
    _assert_rule_clean("R4")


def test_r5_leaf_purity() -> None:
    """R5：common/media/schema 不依赖叶子组之外的 wing 包（规则内例外除外）。"""
    _assert_rule_clean("R5")


def test_r6_root_has_no_side_effects() -> None:
    """R6：wing/__init__.py 不 import 任何 wing.* 子模块。"""
    _assert_rule_clean("R6")


def test_import_normalization_shapes() -> None:
    """形态级元测试：各类 import 写法 → ``(目标模块, 符号名)`` 的归一化结果。

    覆盖 review S1 的包属性形态（``from wing import store``）、相对形态边界与
    ``module=None`` 退回包根的形态（用例形状借 ``libs/wing-probe/tests/test_guard.py``）。
    """
    cases: list[tuple[str, str, list[tuple[str, tuple[str, ...]]]]] = [
        # from wing import <子模块>：必须解析成子模块（S1），不是包根
        (
            "from wing import store\n",
            "wing/tools/probe.py",
            [("wing.store", ("store",))],
        ),
        (
            "from wing import store as s\n",
            "wing/tools/probe.py",
            [("wing.store", ("store",))],
        ),
        (
            "from wing import session, store\n",
            "wing/tools/probe.py",
            [("wing.session", ("session",)), ("wing.store", ("store",))],
        ),
        (
            "from wing import gateway\n",
            "wing/agent/probe.py",
            [("wing.gateway", ("gateway",))],
        ),
        # from wing import <符号>（不是子模块）：退回包根（wing/__init__ re-export 的现状）
        (
            "from wing import execute_shell\n",
            "wing/tools/probe.py",
            [("wing", ("execute_shell",))],
        ),
        # 常规形态
        ("from wing.store import *\n", "wing/tools/probe.py", [("wing.store", ("*",))]),
        (
            "import wing.store.base as sb\n",
            "wing/tools/probe.py",
            [("wing.store.base", ("wing.store.base",))],
        ),
        (
            "import wing.agent\n",
            "wing/tools/probe.py",
            [("wing.agent", ("wing.agent",))],
        ),
        (
            "from wing.agent import ToolContext, current_tool_call_id\n",
            "wing/tools/probe.py",
            [("wing.agent", ("ToolContext", "current_tool_call_id"))],
        ),
        # 相对形态
        (
            "from ..store import base\n",
            "wing/gateway/probe.py",
            [("wing.store", ("base",))],
        ),
        (
            "from . import base, react\n",
            "wing/event/__init__.py",
            [
                ("wing.event.base", ("base",)),
                ("wing.event.react", ("react",)),
            ],
        ),
        (
            "from . import utils\n",
            "wing/common/with_retry.py",
            [("wing.common.utils", ("utils",))],
        ),
        # module=None 且名字不是子模块 → 退回所在包（符号 re-export 形态）
        (
            "from . import NOT_A_MODULE\n",
            "wing/common/probe.py",
            [("wing.common", ("NOT_A_MODULE",))],
        ),
    ]
    problems: list[str] = []
    for source, rel_path, expected in cases:
        actual = [
            (resolved.dst_module, resolved.names)
            for resolved in _iter_imports(ast.parse(source), rel_path)
        ]
        if actual != expected:
            problems.append(
                f"{source.strip()!r} @ {rel_path}\n      期望 {expected}\n      实得 {actual}"
            )
    if problems:
        pytest.fail(
            "[归一化] import 形态解析不符预期：\n"
            + "\n".join(f"  {problem}" for problem in problems),
            pytrace=False,
        )


def test_bypass_shapes_are_detected() -> None:
    """评审 S1/S2 的绕过形态必须命中对应规则（合成片段级回归）。"""
    cases: list[tuple[str, str, set[str]]] = [
        # S1：包属性形态（修复前：R1/R2/R4 完全漏判、R5 错报成 → wing）
        ("from wing import store\n", "wing/tools/probe.py", {"R2"}),
        ("from wing import store as s\n", "wing/tools/probe.py", {"R2"}),
        ("from wing import gateway\n", "wing/agent/probe.py", {"R1"}),
        ("from wing import session\n", "wing/store/probe.py", {"R4"}),
        ("from wing import tools\n", "wing/common/probe.py", {"R5"}),
        # S2：WingAgent 等包根越层符号必须命中；窄接口符号不命中
        ("from wing.agent import WingAgent\n", "wing/tools/probe.py", {"R2"}),
        ("from wing.agent import Inbound\n", "wing/tools/probe.py", {"R2"}),
        ("from wing.agent import *\n", "wing/tools/probe.py", {"R2"}),
        ("import wing.agent\n", "wing/tools/probe.py", {"R2"}),
        (
            "from wing.agent import ToolContext, current_tool_call_id\n",
            "wing/tools/probe.py",
            set(),
        ),
        (
            "from wing.agent import tool_context\n",
            "wing/tools/probe.py",
            set(),
        ),
        # N1（r2）：R5 例外只对**函数体内**懒加载生效——同一对写成模块级必须变红
        ("from wing.config import get_config\n", "wing/common/logger.py", {"R5"}),
        (
            "def f():\n    from wing.config import get_config\n",
            "wing/common/logger.py",
            set(),
        ),
        # 合法形态不应误报
        ("from wing import schema\n", "wing/common/probe.py", set()),
        ("import os\n", "wing/tools/probe.py", set()),
    ]
    problems: list[str] = []
    for source, rel_path, expected in cases:
        actual = _rule_hits_for_source(source, rel_path)
        if actual != expected:
            problems.append(
                f"{source.strip()!r} @ {rel_path}\n      期望 {sorted(expected)}"
                f"\n      实得 {sorted(actual)}"
            )
    if problems:
        pytest.fail(
            "[绕过形态] 规则判定不符预期：\n"
            + "\n".join(f"  {problem}" for problem in problems),
            pytrace=False,
        )


def test_leaf_exceptions_are_function_local() -> None:
    """R5 例外登记的是「函数内懒加载」：条目必须真实命中，且命中处位于函数体内。"""
    scan = _scan()
    problems: list[str] = []
    for src_rel, dst_module in sorted(LEAF_LAZY_EXCEPTIONS):
        matching = [
            edge
            for edge in scan.edges
            if edge.src_rel == src_rel and edge.dst_module == dst_module
        ]
        if not matching:
            problems.append(
                f"{src_rel} → {dst_module}: 例外未命中任何 import（已失效，请删除）"
            )
        elif not any(edge.in_function for edge in matching):
            problems.append(
                f"{src_rel} → {dst_module}: 命中的 import 不在函数体内——例外只对懒加载生效"
            )
    if problems:
        pytest.fail(
            "[R5] 例外观测异常：\n" + "\n".join(f"  {problem}" for problem in problems),
            pytrace=False,
        )
