"""分层守门 —— AST 解析 ``libs/core/wing/**`` 的 import 关系，锁定目标分层。

规则、分层图与迁移映射见 ``docs/dev/backend-layout.md``。本文件是"可执行的那一份"。

设计要点
--------
- **纯 AST，不 import wing**：解析源码文本而非导入源码，规则判定与包能否成功导入无关。
- 收集每个 ``.py`` 的 ``ast.Import`` / ``ast.ImportFrom``：模块级、``if TYPE_CHECKING:``
  块、函数内懒加载**一律计入**——import 即依赖，懒加载只是延迟代价，不解除依赖。
- 相对 import 按文件所在包绝对化；``from . import x`` 视为依赖子模块 ``wing.x``。
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
   agent 侧只允许 ``wing.agent``（公共 re-export）与 ``wing.agent.tool_context``。
R3 provider 独立：``provider`` 不得 import agent/session/context/runtime/gateway/tools。
R4 存储不反向：``store`` 不得 import L3/L4 任一族。
R5 叶子纯净：``wing/common/**``、``wing/media*``、``wing/schema*`` 不得 import 叶子组
   ``{common, media, schema}`` 之外的 wing 包（规则内例外见 ``LEAF_LAZY_EXCEPTIONS``）。
R6 顶层无副作用：``wing/__init__.py`` 不得 import 任何 ``wing.*`` 子模块。
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

# R5 规则内例外（精确到「源文件 → 目标模块」）：三条函数内懒加载是设计选择
# （import 期无结构依赖、无加载副作用），且无重构步骤负责——登记为规则的一部分，
# 而不是"待清理违规"。其它任何跨组依赖（模块级或懒加载）照常变红。
# 详情见 docs/dev/backend-layout.md §规则内例外。
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


def _resolve_from(node: ast.ImportFrom, rel_path: str) -> list[str]:
    """``ImportFrom`` → 目标模块列表；相对 import 按文件所在包绝对化。

    - ``from a.b import c``      -> ``["a.b"]``（只记模块，不展开符号）
    - ``from . import x, y``     -> ``["<包>.x", "<包>.y"]``（module=None 时按子模块记）
    """
    if node.level == 0:
        return [node.module] if node.module else []
    # 文件所在包：foo/bar.py -> ["foo"]；foo/__init__.py -> ["foo"]
    pkg_parts = list(Path(rel_path).with_suffix("").parts)[:-1]
    if node.level > len(pkg_parts):
        return []  # 越出包边界（正常代码不会出现）
    base = pkg_parts[: len(pkg_parts) - (node.level - 1)]
    if node.module is None:
        return [".".join(base + [alias.name]) for alias in node.names]
    return [".".join(base + node.module.split("."))]


def _iter_imports(
    tree: ast.AST, rel_path: str
) -> Iterator[tuple[ast.Import | ast.ImportFrom, list[str]]]:
    """遍历全部 import 语句（含 TYPE_CHECKING 块与函数内懒加载）及其目标模块。"""
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            yield node, [alias.name for alias in node.names]
        elif isinstance(node, ast.ImportFrom):
            yield node, _resolve_from(node, rel_path)


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
        for node, dst_modules in _iter_imports(tree, rel):
            for dst in dst_modules:
                if not (dst == "wing" or dst.startswith("wing.")):
                    continue  # 非 wing 目标（标准库 / 第三方）
                if _family_of(dst) is None:
                    problems.append(
                        f"[R0] {rel}:{node.lineno} → {dst}: 目标模块未登记"
                        f"（模块不存在或新包未登记 FAMILY_RULES）"
                    )
                    continue
                edges.append(
                    ImportEdge(rel, node.lineno, dst, _stmt_text(source, node))
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


def _r1(ctx: ViolationContext) -> bool:
    """R1 传输隔离：L0–L3 与 root 不得 import wing.gateway.*（runtime/background 例外）。"""
    return ctx.dst_family == "gateway" and ctx.src_family not in {
        "gateway",
        "runtime",
        "background",
    }


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
    # agent 侧：允许包公共 re-export（wing.agent）与窄接口模块，禁止其它子模块。
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
    """R5 叶子纯净（规则内例外见 LEAF_LAZY_EXCEPTIONS）。"""
    if not _is_leaf_path(ctx.src_rel):
        return False
    if ctx.dst_family in LEAF_GROUP:
        return False
    return (ctx.src_rel, ctx.dst_module) not in LEAF_LAZY_EXCEPTIONS


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
    "R2": {
        # cleared by 03_delete_legacy_tools（Explorer 整体删除）
        "wing/tools/explorer.py:21 → wing.context_manager",
        # cleared by 03_delete_legacy_tools（Explorer 整体删除）
        "wing/tools/explorer.py:25 → wing.store",
        # cleared by 03_delete_legacy_tools（Explorer 整体删除）
        "wing/tools/explorer.py:29 → wing.agent.core",
        # cleared by 03_delete_legacy_tools（Explorer 整体删除）
        "wing/tools/explorer.py:81 → wing.agent.core",
    },
    "R5": {
        # cleared by 07_split_session_chain（tracked_list 迁出 wing/common/ → wing/chain.py，不再受 R5 约束）
        "wing/common/tracked_list.py:28 → wing.store.base",
        # cleared by 07_split_session_chain（同上；chain 依赖 event 注册表是设计允许项）
        "wing/common/tracked_list.py:171 → wing.event",
    },
    "R6": {
        # cleared by 06_public_api（顶层 wing/__init__ 无副作用化）
        "wing/__init__.py:1 → wing.tools",
        # cleared by 06_public_api（同上）
        "wing/__init__.py:2 → wing.metrics_registry",
    },
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
            edge.src_rel, src.name, edge.dst_module, dst.name, dst.layer
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
