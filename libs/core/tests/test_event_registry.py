"""门禁：EVENT_TYPES 派生集合 == 事件模块里声明的全部 `type` 字面量。

注册表从类派生（`type` 的唯一声明处是每个类的字面量字段），所以这里用**独立
来源**交叉验证：扫 `wing/event/*.py` 的 AST，收集每个类 `type: Literal["…"]`
注解里的字符串。这条断言覆盖派生本身看不到的失效——新增事件类**没被
`wing/event/__init__.py` import**：那时派生表里没有它，`TrackedList.load` 对未知
type 静默跳过，症状是重启后该事件从 resume 重放里消失。
"""

from __future__ import annotations

import ast
from pathlib import Path

import wing.event as event_pkg
from wing.event import EVENT_TYPES, FACT_EVENTS, WingEvent

_PACKAGE_DIR = Path(event_pkg.__file__).parent


def _declared_event_types() -> dict[str, str]:
    """{type 字面量: 类名}——直接读源码 AST，不依赖运行期类层次。"""
    declared: dict[str, str] = {}
    for path in sorted(_PACKAGE_DIR.glob("*.py")):
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        for node in tree.body:
            if not isinstance(node, ast.ClassDef):
                continue
            for stmt in node.body:
                if not isinstance(stmt, ast.AnnAssign):
                    continue
                target, annotation = stmt.target, stmt.annotation
                if not isinstance(target, ast.Name) or target.id != "type":
                    continue
                if not isinstance(annotation, ast.Subscript):
                    continue
                if (
                    not isinstance(annotation.value, ast.Name)
                    or annotation.value.id != "Literal"
                ):
                    continue
                literal = annotation.slice
                if isinstance(literal, ast.Constant) and isinstance(literal.value, str):
                    declared[literal.value] = node.name
    return declared


def test_scanner_finds_declarations() -> None:
    """扫描器自身要有产出（否则下面的等价断言会真空通过）。"""
    declared = _declared_event_types()
    assert len(declared) >= 20, f"AST scan found only {len(declared)} event types"


def test_registry_matches_declared_literals() -> None:
    declared = _declared_event_types()
    missing = sorted(set(declared) - set(EVENT_TYPES))
    extra = sorted(set(EVENT_TYPES) - set(declared))
    assert not missing, (
        f"declared but not in EVENT_TYPES (class not imported by wing/event/__init__.py?): {missing}"
    )
    assert not extra, f"in EVENT_TYPES but no declared literal: {extra}"


def test_registry_maps_type_to_declaring_class() -> None:
    """每个键映射到的类 == 源码里声明该字面量的类（防跨类串键）。"""
    declared = _declared_event_types()
    mismatched = {
        type_name: (EVENT_TYPES[type_name].__name__, class_name)
        for type_name, class_name in declared.items()
        if EVENT_TYPES[type_name].__name__ != class_name
    }
    assert not mismatched, f"type → class mismatch: {mismatched}"


def test_every_registered_class_is_an_event() -> None:
    for cls in EVENT_TYPES.values():
        assert issubclass(cls, WingEvent)


# 落盘（persist=True）但不是「链上事实」的事件：重放它们会给前端制造一次假的
# "刚刚发生"。每条都要写清为什么不算事实。
_PERSISTED_NOT_REPLAYED: dict[str, str] = {
    "done": "轮结束信号（一次性，不是链上状态）",
    "turn_started": "轮开始信号（一次性）",
    "user_message_accepted": "对某条入站消息的消费确认（ack）",
    "turn_result": "轮级结果（duration / usage / errors 是审计字段）",
}


def test_fact_events_are_the_replayable_persisted_events() -> None:
    """FACT_EVENTS == 「persist=True 的 type」− 显式列出的「落盘但不重放」。

    等式而不是包含：新增一个 persist=True 的事件时必须做一次决定——它是链上事实
    （进 FACT_EVENTS，resume 时重放）还是轮级 / 一次性信号（进上面的登记表）。
    漏做决定会红。
    """
    persisted = {type_name for type_name, cls in EVENT_TYPES.items() if cls.persist}
    assert set(FACT_EVENTS) == persisted - set(_PERSISTED_NOT_REPLAYED)

    # 反方向：登记表里的 type 必须仍是「落盘事件」——否则它是一条没有对应事实的
    # 死行（类被删 / persist 改成 False 时不会有别人发现）。
    orphan = set(_PERSISTED_NOT_REPLAYED) - persisted
    assert not orphan, f"登记表里的 type 已不是落盘事件: {sorted(orphan)}"
