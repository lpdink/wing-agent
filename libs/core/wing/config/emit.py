# wing/config/emit.py
"""规范形 YAML emitter —— 默认模板与保存路径**共用**的唯一实现。

为什么自己写（不用 ``yaml.dump`` 输出结构）：``yaml.dump`` 不能输出注释、不能控制键序、
更不能做「缺席即默认 → 注释掉的默认值」这件事。默认模板与保存写盘从此都只有一条路径。

**文件里只有两类注释**：分组分隔行与注释掉的默认值（另有两处安全注记：未知键标记、
以及只有展示路径才会产生的掩码提示——后者不在本文件的落盘路径上）。字段的 ``doc`` /
``notes`` / ``example`` / 生效域 / 密文说明**一概不写进文件**——它们的归宿是设置面板与
``wing config``（同一个声明层，见 ``spec.SettingMeta``）；文件只承载数据与可发现性
（这个键存在、它的默认值是什么）。散文注释把几十行的配置撑成上千行，是文件难读的主因。

输出规则：

1. 文件头 1 行（产品名 + 文件位置）；**不写时间戳**——它会让每次保存都产生 diff 噪声；
2. 按业务分组（``config/groups.py`` 的表盖到目录节点上的 ``section``）分段，每组一行
   ``# ── Name ───…`` 分隔；**键序恒为模型声明序**，分隔行按组的首次出现发一次
   （分组表管界面顺序，不管文件顺序）；
3. 每个字段：值存在 → 写出；值缺席且默认有值 → **注释掉的默认值**（object 整棵子树注释）；
   值缺席且默认是 ``null`` → 不写（``null`` 没有可展示的事实）；值缺席且必填 → 键 + 空值
   （天然的 problem）；
4. 列表项只展开**第一项**的缺席默认值：条目级开关一次可发现，又不随条目数膨胀；
5. schema 之外的未知键（``extra``）在父容器末尾原样写回 + 标记注释（前向兼容）；
6. 字符串引号策略：能不加就不加（判据 = 「不加引号能否原样读回」），需要时双引号 + 转义；
7. 缩进 2 空格；列表项 ``- `` 缩进在父键下一级（与旧模板一致的形态）。

不变量：**取消注释即一份合法的值**——注释块里的每一行都是它所在键的完整默认值，
空容器显式写 ``{}`` / ``[]``（只写 ``key:`` 是 YAML null，round-trip 会破）。

纯逻辑：不读文件、不 import gateway / runtime / event_bus；结构脚本自己拼，只借 PyYAML 序列化
**单个标量与原始载荷**（引号 / 转义 / 数值形态交给它，那是它最擅长的部分）。
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING, Any

import yaml

from .catalog import SettingKind, SettingNode, child_path, item_template

if TYPE_CHECKING:
    from .document import ExtraKey

_MISSING = object()
"""稀疏文档里的「键缺席」（与显式 ``None`` 区分：``null`` 是用户写下的值）。"""

_HEADER = "# wing-agent configuration — $WING_HOME/core/config.yaml"
"""文件头（1 行：产品名 + 文件位置）。"""

_SECTION_WIDTH = 64
"""``# ── Name ───…`` 分隔行的总宽度（照旧模板的观感）。"""

_UNKNOWN_KEY_NOTE = "unknown key (not recognized by this wing version)"
"""未知键上方的标记注释（规则 5）。"""


def default_document() -> dict[str, Any]:
    """首次运行的稀疏文档：两个空列表 = **天然的 problem**（设置向导据此指路）。

    其余键全部缺席 → emitter 把它们的默认值写成注释掉的行使文件可发现；没有 ``ChangeHere``
    这种"假值"，也不需要一套特殊的占位符语义。
    """
    return {"providers": [], "agents": []}


def emit_config_yaml(
    data: dict[str, Any],
    catalog: SettingNode,
    extra: list[ExtraKey] | None = None,
) -> str:
    """稀疏文档 + catalog → 规范形 YAML 文本（默认模板与保存写盘共用）。

    Args:
        data: 稀疏文档（只有用户显式写下的键；``{}`` = 空文件）。
        catalog: ``build_catalog()`` 的产物（根节点）。
        extra: schema 之外的未知键（``document.ExtraKey``：父前缀 + 原始键名 + 值）；
            由**父前缀**定位，写在该容器已知键之后（找不到容器的条目兜底追加在文件末尾
            ——绝不丢用户的键）。
    """
    out: list[str] = [_HEADER]
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
    """字符串引号策略（规则 6）：能不加就不加；需要时双引号 + 转义、非 ASCII 原样。"""
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


def _emit_section_banner(out: list[str], node: SettingNode) -> None:
    """``# ── Providers ───…`` 分隔行（规则 2）。"""
    prefix = f"# ── {node.section} "
    out.append(prefix + "─" * max(0, _SECTION_WIDTH - len(prefix)))


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
    allow_defaults: bool = True,
) -> None:
    """一个 object 节点的子字段（``data`` 是它的值；缺席的子字段照规则 3 处理）。

    ``allow_defaults=False`` 只用在**非首项的列表项**里：条目级开关不随条目数膨胀。
    """
    if top_level:
        _emit_sections(out, extras, node, data, prefix, allow_defaults)
        return
    for child in node.children:
        raw = data.get(child.key, _MISSING) if isinstance(data, dict) else _MISSING
        _emit_field(
            out,
            extras,
            child,
            child_path(prefix, child.key),
            raw,
            indent,
            commented=commented,
            allow_defaults=allow_defaults,
        )
    if not commented:
        # 未知键写在父容器末尾（规则 5）；注释态没有文档可承载它们，留给文件末尾兜底。
        _emit_extras(out, extras, prefix, indent)


def _emit_sections(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    data: dict[str, Any],
    prefix: str,
    allow_defaults: bool,
) -> None:
    """顶层：按分组缓冲后分段。

    分组表管界面顺序、文件顺序恒为声明序；`validate_groups` 保证同组成员在声明序里**连续**，
    所以每组恰好一段、一条分隔行。**正文为空的分组连分隔行一起省掉**（一条没有内容的横幅
    只会让人以为文件被截断）。
    """
    children = node.children
    index = 0
    while index < len(children):
        section = children[index].section
        start = index
        while index < len(children) and children[index].section == section:
            index += 1
        body: list[str] = []
        for child in children[start:index]:
            raw = data.get(child.key, _MISSING) if isinstance(data, dict) else _MISSING
            _emit_field(
                body,
                extras,
                child,
                child_path(prefix, child.key),
                raw,
                0,
                commented=False,
                allow_defaults=allow_defaults,
            )
        if not body:
            continue
        if out and out[-1] != "":
            out.append("")
        if section is not None:
            _emit_section_banner(out, children[start])
        out.extend(body)
    _emit_extras(out, extras, prefix, 0)


def _emit_field(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    path: str,
    raw: Any,
    indent: int,
    *,
    commented: bool,
    allow_defaults: bool,
) -> None:
    """一个字段：值（存在 / 注释掉的默认值 / 必填空值）。"""
    if raw is _MISSING:
        if node.required:
            _emit_value(
                out,
                extras,
                node,
                path,
                _empty_value(node),
                indent,
                commented=commented,
                allow_defaults=allow_defaults,
            )
        elif node.has_default and allow_defaults:
            _emit_absent_default(out, extras, node, path, indent, commented=commented)
        # 无值、无默认（或默认是 null）、非必填 ⇒ 没有可展示的事实
        return
    _emit_value(
        out,
        extras,
        node,
        path,
        raw,
        indent,
        commented=commented,
        allow_defaults=allow_defaults,
    )


def _emit_absent_default(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    path: str,
    indent: int,
    *,
    commented: bool,
) -> None:
    """缺席且有默认：标量写成注释掉的默认值；object 把整棵子树注释掉（规则 3 的推广）。

    list 的 ``default`` 在 catalog 里恒为 ``None``（结构由 ``element`` 表达），但展示形态要
    写出可读的默认值——空列表（现状所有 list 默认都是 ``[]``）。
    """
    if node.kind is SettingKind.OBJECT:
        body: list[str] = []
        _emit_children(body, extras, node, {}, indent + 2, path, commented=True)
        if not body:
            # 子树里没有一个可展示的默认值：`key:` 取消注释是 null，必须显式写 `{}`。
            _line(out, indent, f"{_key_text(node.key)}: {{}}", True)
            return
        _line(out, indent, f"{_key_text(node.key)}:", True)
        out.extend(body)
        return
    if node.kind is SettingKind.LIST:
        # catalog 里 list 的 default 恒为 None（结构由 element 表达）；声明了非空默认值时照写。
        default = node.default if isinstance(node.default, list) else []
        _emit_value(out, extras, node, path, default, indent, commented=True)
        return
    if node.default is None:
        return
    _emit_value(out, extras, node, path, node.default, indent, commented=True)


def _emit_value(
    out: list[str],
    extras: _Extras,
    node: SettingNode,
    path: str,
    value: Any,
    indent: int,
    *,
    commented: bool,
    allow_defaults: bool = True,
) -> None:
    """一个字段的取值形态（object / list / map / 标量；``commented`` 时整体注释化）。"""
    key = _key_text(node.key)
    if node.kind is SettingKind.OBJECT and isinstance(value, dict):
        if not value and not extras.pending(path):
            # `key:` 后面只有注释 = YAML null，会把 `{}` 读成 None——空容器必须显式写。
            _line(out, indent, f"{key}: {{}}", commented)
        else:
            _line(out, indent, f"{key}:", commented)
            _emit_children(
                out,
                extras,
                node,
                value,
                indent + 2,
                path,
                commented=commented,
                allow_defaults=allow_defaults,
            )
        return
    if node.kind is SettingKind.LIST and isinstance(value, list):
        if not value:
            _line(out, indent, f"{key}: []", commented)
        else:
            _line(out, indent, f"{key}:", commented)
            for index, item in enumerate(value):
                _emit_list_item(
                    out,
                    extras,
                    node,
                    item,
                    f"{path}[{index}]",
                    indent + 2,
                    commented,
                    allow_defaults=allow_defaults and index == 0,
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
    等于把它弄丢了。
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
    *,
    allow_defaults: bool,
) -> None:
    """一个列表项：对象形态走 ``- key: value`` 合并，标量一项一行，其余原样。"""
    template = item_template(node, item)
    if (
        template is not None
        and template.kind is SettingKind.OBJECT
        and isinstance(item, dict)
    ):
        _emit_object_item(
            out,
            extras,
            template,
            item,
            item_path,
            indent,
            commented,
            allow_defaults=allow_defaults,
        )
    elif isinstance(item, (dict, list)):
        _emit_raw_item(out, item, indent, commented)
    else:
        _line(out, indent, f"- {_scalar_text(item)}", commented)


def _emit_object_item(
    out: list[str],
    extras: _Extras,
    template: SettingNode,
    item: dict[str, Any],
    item_path: str,
    indent: int,
    commented: bool,
    *,
    allow_defaults: bool,
) -> None:
    """对象列表项：``- `` 与第一条非空行合并（注释块的首行是 ``- # …``）。

    不把行提升到 ``- `` 上方：那样后面的项会因为"前一项的行尾"而看起来带着别的项的行。
    """
    if not item:
        _line(out, indent, "- {}", commented)
        return
    body: list[str] = []
    _emit_children(
        body,
        extras,
        template,
        item,
        indent + 2,
        item_path,
        commented=commented,
        allow_defaults=allow_defaults,
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

    与 ``_emit_keyed_raw`` 不同，这里把载荷首行并进 ``- `` 是**正确**的：
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
    """schema 之外的未知键，按**父容器前缀**归类；``take`` 过的父路径算「已归位」。

    消费 ``document.ExtraKey`` 三元组：父前缀在读取时结构性拼接、键名不参与切分
    （键名含点时靠 ``rpartition(".")`` 反推边界会把键挪到错误的名字 /
    位置）。不做任何字符串解析，坏形态在类型层就不可表达。
    """

    def __init__(self, extra: list[ExtraKey] | None) -> None:
        self._by_parent: dict[str, list[tuple[str, Any]]] = {}
        self._consumed: set[str] = set()
        for entry in extra or []:
            self._by_parent.setdefault(entry.parent, []).append(
                (entry.key, entry.value)
            )

    def pending(self, parent: str) -> bool:
        """这个父容器还有没写回的未知键？（决定空对象写成 `{}` 还是块）"""
        return bool(self._by_parent.get(parent))

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
