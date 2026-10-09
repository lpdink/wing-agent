# wing/config/emit.py
"""规范形 YAML emitter —— 默认模板与保存路径**共用**的唯一实现。

为什么自己写（不用 ``yaml.dump`` 输出结构）：``yaml.dump`` 不能输出注释、不能控制键序、
更不能做「缺席即默认 → 注释掉的默认值」这件事。而这份文件的注释全部来自声明
（``SettingMeta`` 的 ``doc`` / ``notes`` / ``example`` / ``apply`` / ``secret``），
是「两份镜像靠 ``# SYNC`` 维系」这段历史的终点：默认模板与保存写盘从此都只有一条路径。

输出规则（总设计 §6.2 的八条，产品品味，逐条落地）：

1. 文件头 4 行注释（产品名 / 文件位置 / 「推荐在 TUI 里用 ``/settings`` 编辑」；**不写时间戳**——
   它会让每次保存都产生 diff 噪声）；
2. 按 ``section`` 分组（声明序连续），每组一行 ``# ── Name ───…`` 分隔 + ``section_doc`` 块注释；
3. 每个字段：``doc`` / ``notes`` / ``example`` 在上方（不用行尾注释）；值存在 → 写出；
   值缺席且有默认 → **注释掉的默认值**；值缺席且必填 → 键 + 空值（天然的 problem，
   ``ChangeHere`` 占位符彻底死亡）；
4. ``secret`` 字段加一行「只写不回显」说明（值本身照写——文件本来就是明文密钥的家）；
5. ``apply`` 为 ``restart`` / ``next_session`` 时加一行生效域标记（``hot`` 是默认，不加噪声）；
6. schema 之外的未知键（``extra``）在父容器末尾原样写回 + 标记注释（前向兼容）；
7. 字符串引号策略：能不加就不加（判据 = 「不加引号能否原样读回」），需要时双引号 + 转义；
8. 缩进 2 空格；列表项 ``- `` 缩进在父键下一级（与旧模板一致的形态）。

纯逻辑：不读文件、不 import gateway / runtime / event_bus；结构脚本自己拼，只借 PyYAML 序列化
**单个标量与原始载荷**（引号 / 转义 / 数值形态交给它，那是它最擅长的部分）。
"""

from __future__ import annotations

import json
from typing import Any

import yaml

from .catalog import SettingKind, SettingNode
from .spec import ApplyScope

_MISSING = object()
"""稀疏文档里的「键缺席」（与显式 ``None`` 区分：``null`` 是用户写下的值）。"""

_HEADER: tuple[str, ...] = (
    "# ──────────────────────────────────────────────────────────────",
    "# wing-agent configuration",
    "# Location: $WING_HOME/core/config.yaml (default: ~/.wing/core/config.yaml)",
    "# 推荐在 TUI 里用 /settings 编辑（本文件由声明生成，未写出的键跟随默认）。",
)

_SECTION_WIDTH = 64
"""``# ── Name ───…`` 分隔行的总宽度（照旧模板的观感）。"""

_UNKNOWN_KEY_NOTE = "unknown key (not recognized by this wing version)"
"""未知键上方的标记注释（§6.2 规则 6）。"""


def default_document() -> dict[str, Any]:
    """首次运行的稀疏文档：两个空列表 = **天然的 problem**（04 的 setup 向导据此指路）。

    其余键全部缺席 → emitter 把它们的默认值写成注释掉的行使文件自文档；没有 ``ChangeHere``
    这种"假值"，也不需要一套特殊的占位符语义（总设计 §20 D24）。
    """
    return {"providers": [], "agents": []}


def emit_config_yaml(
    data: dict[str, Any],
    catalog: SettingNode,
    extra: list[tuple[str, Any]] | None = None,
) -> str:
    """稀疏文档 + catalog → 规范形 YAML 文本（默认模板与保存写盘共用）。

    Args:
        data: 稀疏文档（只有用户显式写下的键；``{}`` = 空文件）。
        catalog: ``build_catalog()`` 的产物（根节点）。
        extra: schema 之外的未知键 ``(规范路径, 原始值)``；由其**父路径**定位，
            写在该容器已知键之后（找不到容器的条目兜底追加在文件末尾——绝不丢用户的键）。
    """
    out: list[str] = list(_HEADER)
    out.append("")
    extras = _Extras(extra)
    body = data if isinstance(data, dict) else {}
    _emit_children(out, extras, catalog, body, 0, "", top_level=True)
    for name, value in extras.leftovers():
        out.append("")
        _comment(out, 0, _UNKNOWN_KEY_NOTE)
        _emit_keyed_raw(out, name, value, 0, commented=False)
    return "\n".join(out).rstrip("\n") + "\n"


# ─────────────────────────────────────────────────────────────────────────────
# 行级原语（注释行 / 值行 / 缩进）
# ─────────────────────────────────────────────────────────────────────────────


def _line(out: list[str], indent: int, text: str, commented: bool) -> None:
    """一条输出行；``commented`` 时在行首插 ``# ``（整块注释态 / 注释掉的默认值）。"""
    out.append(f"{' ' * indent}# {text}" if commented else f"{' ' * indent}{text}")


def _comment(out: list[str], indent: int, text: str) -> None:
    """一条注释行（两种态下都一样：注释就是注释）。"""
    out.append(f"{' ' * indent}# {text}")


# ─────────────────────────────────────────────────────────────────────────────
# 标量与原始载荷
# ─────────────────────────────────────────────────────────────────────────────


def _scalar_text(value: Any) -> str:
    """单个标量 → YAML 文本（字符串走引号策略，其余交给 PyYAML）。"""
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return _string_scalar(value)
    return _yaml_scalar(value)


def _string_scalar(text: str) -> str:
    """字符串引号策略（规则 7）：能不加就不加；需要时双引号 + 转义、非 ASCII 原样。"""
    if _plain_safe(text):
        return text
    return json.dumps(text, ensure_ascii=False)


def _plain_safe(text: str) -> bool:
    """不加引号能否原样读回？判据就是「读回来是否等于自己」——比手写特殊字符表可靠。"""
    if not text:
        return False
    try:
        loaded = yaml.safe_load(text)
    except yaml.YAMLError:
        return False
    return isinstance(loaded, str) and loaded == text


def _yaml_scalar(value: Any) -> str:
    """非字符串标量：交 PyYAML（数值形态 / 转义由它保证 round-trip），剥掉 ``...`` 结束标记。"""
    return _strip_end(yaml.safe_dump(value, allow_unicode=True))


def _raw_yaml(value: Any) -> str:
    """任意值的 YAML 载荷（map 的值 / 未知键的值）：块风格、保留键序。"""
    return _strip_end(
        yaml.safe_dump(
            value, allow_unicode=True, default_flow_style=False, sort_keys=False
        )
    )


def _strip_end(text: str) -> str:
    """``safe_dump`` 对标量文档会补一行 ``...``（文档结束标记）：去掉。"""
    if text.endswith("...\n"):
        text = text[:-4]
    return text.rstrip("\n")


def _key_text(key: str) -> str:
    """键名文本（与字符串值同一份引号策略：键也可能需要引号）。"""
    return _string_scalar(key)


# ─────────────────────────────────────────────────────────────────────────────
# 字段 / 容器
# ─────────────────────────────────────────────────────────────────────────────


def _comment_lines(node: SettingNode) -> list[str]:
    """一个字段的文档注释块（规则 3/4/5）：doc → notes → example → 生效域 → 密钥。"""
    lines: list[str] = []
    if node.doc:
        lines.append(node.doc)
    lines.extend(node.notes)
    if node.example:
        lines.append(f"e.g. {node.example}")
    if node.apply is ApplyScope.RESTART:
        lines.append("生效：需重启网关")
    elif node.apply is ApplyScope.NEXT_SESSION:
        lines.append("生效：新会话")
    if node.secret:
        lines.append("密钥：面板里只写不回显（末 4 位提示）")
    return lines


def _emit_section_banner(out: list[str], node: SettingNode) -> None:
    """``# ── Providers ───…`` + ``section_doc``（规则 2）。"""
    prefix = f"# ── {node.section} "
    out.append(prefix + "─" * max(0, _SECTION_WIDTH - len(prefix)))
    if node.section_doc:
        for line in node.section_doc.splitlines():
            _comment(out, 0, line)


def _emit_children(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    data: dict[str, Any],
    indent: int,
    prefix: str,
    *,
    top_level: bool = False,
    commented: bool = False,
) -> None:
    """一个 object 节点的子字段（``data`` 是它的值；缺席的子字段照规则 3 处理）。"""
    section: str | None = None
    emitted = 0
    for child in node.children:
        child_path = _child_path(prefix, child.key)
        if top_level:
            if emitted:
                out.append("")
            if child.section is not None and child.section != section:
                section = child.section
                _emit_section_banner(out, child)
        raw = data.get(child.key, _MISSING) if isinstance(data, dict) else _MISSING
        _emit_field(out, extras, child, child_path, raw, indent, commented=commented)
        emitted += 1
    if not commented:
        # 未知键写在父容器末尾（§6.2 规则 6）；注释态没有文档可承载它们，留给文件末尾兜底。
        _emit_extras(out, extras, prefix, indent)


def _emit_field(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    path: str,
    raw: Any,
    indent: int,
    *,
    commented: bool,
) -> None:
    """一个字段：文档注释块 + 值（存在 / 注释掉的默认值 / 必填空值）。"""
    for line in _comment_lines(node):
        _comment(out, indent, line)
    if raw is _MISSING:
        if node.required:
            _emit_value(
                out, extras, node, path, _empty_value(node), indent, commented=commented
            )
        elif node.has_default:
            _emit_absent_default(out, extras, node, path, indent, commented=commented)
        # 无值、无默认、非必填 ⇒ 没有可展示的事实（Interface 根才有这种字段）
        return
    _emit_value(out, extras, node, path, raw, indent, commented=commented)


def _emit_absent_default(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    path: str,
    indent: int,
    *,
    commented: bool,
) -> None:
    """缺席且有默认：标量写成注释掉的默认值；object 把整棵子树注释掉（§6.2 规则 3 的推广）。

    list 的 ``default`` 在 catalog 里恒为 ``None``（结构由 ``element`` 表达），但展示形态要
    写出可读的默认值——空列表（现状所有 list 默认都是 ``[]``）。
    """
    if node.kind is SettingKind.OBJECT:
        _line(out, indent, f"{_key_text(node.key)}:", True)
        _emit_children(out, extras, node, {}, indent + 2, path, commented=True)
        return
    default = node.default
    if default is None and node.kind is SettingKind.LIST:
        default = []
    _emit_value(out, extras, node, path, default, indent, commented=True)


def _emit_value(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    path: str,
    value: Any,
    indent: int,
    *,
    commented: bool,
) -> None:
    """一个字段的取值形态（object / list / map / 标量；``commented`` 时整体注释化）。"""
    key = _key_text(node.key)
    if node.kind is SettingKind.OBJECT and isinstance(value, dict):
        if not value:
            # `key:` 后面只有注释 = YAML null，会把 `{}` 读成 None——空容器必须显式写。
            _line(out, indent, f"{key}: {{}}", commented)
        else:
            _line(out, indent, f"{key}:", commented)
            _emit_children(
                out, extras, node, value, indent + 2, path, commented=commented
            )
        return
    if node.kind is SettingKind.LIST and isinstance(value, list):
        if not value:
            _line(out, indent, f"{key}: []", commented)
        else:
            _line(out, indent, f"{key}:", commented)
            for index, item in enumerate(value):
                _emit_list_item(
                    out, extras, node, item, f"{path}[{index}]", indent + 2, commented
                )
    elif isinstance(value, (dict, list)):
        _emit_keyed_raw(out, node.key, value, indent, commented)
    else:
        _line(out, indent, f"{key}: {_scalar_text(value)}", commented)


def _emit_keyed_raw(
    out: list[str], key: str, value: Any, indent: int, commented: bool
) -> None:
    """键 + 任意 YAML 载荷（freeform map 的值 / 未知键的值）。

    内联（`key: <载荷>`）只对**标量**与**空容器**成立：`safe_dump` 给它们的单行是真·单行
    （`true` / `{}` / `[]`）。非空容器的单行 dump（`a: 1` / `- 1`）是**块结构的首行**，
    直接拼在 `key: ` 后会产出非法 YAML（`key: a: 1` / `key: - 1`，`yaml.safe_load` 直接
    ScannerError）——这类载荷一律走缩进块。前向兼容承诺「原样写回」，写出一份解析不了的文件
    等于把它弄丢了（B1 回归，见 02 design.md「Rework r1」）。
    """
    text = _raw_yaml(value)
    lines = text.split("\n")
    inline = len(lines) == 1 and (not isinstance(value, (dict, list)) or not value)
    if inline:
        _line(out, indent, f"{_key_text(key)}: {lines[0]}", commented)
        return
    _line(out, indent, f"{_key_text(key)}:", commented)
    for line in lines:
        if line:
            _line(out, indent + 2, line, commented)
        else:
            out.append("")


# ─────────────────────────────────────────────────────────────────────────────
# 列表项
# ─────────────────────────────────────────────────────────────────────────────


def _emit_list_item(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    item: Any,
    item_path: str,
    indent: int,
    commented: bool,
) -> None:
    """一个列表项：对象形态走 ``- key: value`` 合并，标量一项一行，其余原样。"""
    template = _item_template(node, item)
    if (
        template is not None
        and template.kind is SettingKind.OBJECT
        and isinstance(item, dict)
    ):
        _emit_object_item(out, extras, template, item, item_path, indent, commented)
    elif isinstance(item, (dict, list)):
        _emit_raw_item(out, item, indent, commented)
    else:
        _line(out, indent, f"- {_scalar_text(item)}", commented)


def _item_template(node: SettingNode, item: Any) -> SettingNode | None:
    """按文档实际值的 JSON 形状选元素形态（与 Rust 侧 ``select_variant`` 同口径）。"""
    if node.element is not None:
        return node.element
    for variant in node.variants or []:
        if _accepts_shape(variant.kind, item):
            return variant
    return None


def _accepts_shape(kind: SettingKind, value: Any) -> bool:
    """``kind`` 与 JSON 形状是否相符（未知 kind 一律不符——形态未知就选不出编辑器）。"""
    if kind in (SettingKind.STR, SettingKind.ENUM, SettingKind.SECRET):
        return isinstance(value, str)
    if kind is SettingKind.INT:
        return isinstance(value, int) and not isinstance(value, bool)
    if kind is SettingKind.FLOAT:
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if kind is SettingKind.BOOL:
        return isinstance(value, bool)
    if kind in (SettingKind.MAP, SettingKind.OBJECT):
        return isinstance(value, dict)
    if kind is SettingKind.LIST:
        return isinstance(value, list)
    return False


def _emit_object_item(
    out: list[str],
    extras: _Extras,
    template: SettingNode,
    item: dict[str, Any],
    item_path: str,
    indent: int,
    commented: bool,
) -> None:
    """对象列表项：``- `` 与第一条非空行合并（可能是该字段的文档注释——``- # doc``）。

    不把注释提升到 ``- `` 上方：那样后面的项会因为"前一项的行尾"而看起来带着别的项的注释。
    """
    if not item:
        _line(out, indent, "- {}", commented)
        return
    body: list[str] = []
    _emit_children(
        body, extras, template, item, indent + 2, item_path, commented=commented
    )
    merge_at = next((index for index, line in enumerate(body) if line.strip()), 0)
    for index, line in enumerate(body):
        if index != merge_at:
            out.append(line)
            continue
        content = line[indent + 2 :] if line.startswith(" " * (indent + 2)) else line
        if commented and content.startswith("# "):
            content = content[2:]
        dash = "# - " if commented else "- "
        out.append(f"{' ' * indent}{dash}{content}")


def _emit_raw_item(out: list[str], item: Any, indent: int, commented: bool) -> None:
    """没有元素声明（或形状对不上）的列表项：原样写出。

    与 ``_emit_keyed_raw`` 不同，这里把载荷首行并进 ``- `` 是**正确**的（不是同一条 B1 缺陷）：
    嵌套序列的紧凑形态 ``- - 1`` 就是 ``[[1]]`` 的合法写法——``yaml.safe_dump([[1]])``
    自己也这么写（实测变异验证：改成分行缩进，解析结果不变）。
    """
    lines = _raw_yaml(item).split("\n")
    for index, line in enumerate(lines):
        if index == 0:
            _line(out, indent, f"- {line}", commented)
        elif line:
            _line(out, indent + 2, line, commented)
        else:
            out.append("")


# ─────────────────────────────────────────────────────────────────────────────
# 未知键（前向兼容）
# ─────────────────────────────────────────────────────────────────────────────


class _Extras:
    """schema 之外的未知键，按父路径归类；``take`` 过的父路径算「已归位」。"""

    def __init__(self, extra: list[tuple[str, Any]] | None) -> None:
        self._by_parent: dict[str, list[tuple[str, Any]]] = {}
        self._consumed: set[str] = set()
        for path, value in extra or []:
            parent, _, key = path.rpartition(".")
            if not key:
                continue
            self._by_parent.setdefault(parent, []).append((key, value))

    def take(self, parent: str) -> list[tuple[str, Any]]:
        self._consumed.add(parent)
        return self._by_parent.get(parent, [])

    def leftovers(self) -> list[tuple[str, Any]]:
        """父容器没走到（父对象本身缺席）的条目：兜底写回文件末尾，绝不丢。"""
        return [
            entry
            for parent, entries in self._by_parent.items()
            if parent not in self._consumed
            for entry in entries
        ]


def _emit_extras(out: list[str], extras: _Extras, parent: str, indent: int) -> None:
    for key, value in extras.take(parent):
        _comment(out, indent, _UNKNOWN_KEY_NOTE)
        _emit_keyed_raw(out, key, value, indent, commented=False)


# ─────────────────────────────────────────────────────────────────────────────
# 空值 / 路径
# ─────────────────────────────────────────────────────────────────────────────


def _empty_value(node: SettingNode) -> Any:
    """「值缺席且必填」写出的空值（按 kind）：让它成为一个能指路的 problem。"""
    if node.kind is SettingKind.LIST:
        return []
    if node.kind is SettingKind.OBJECT or node.kind is SettingKind.MAP:
        return {}
    if node.kind is SettingKind.BOOL:
        return False
    if node.kind is SettingKind.INT:
        return 0
    if node.kind is SettingKind.FLOAT:
        return 0.0
    return ""


def _child_path(prefix: str, key: str) -> str:
    """子路径：root 的子树**不带根前缀**（与 catalog 的路径生成同一条规则）。"""
    return f"{prefix}.{key}" if prefix else key
