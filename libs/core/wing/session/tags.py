# wing/session/tags.py — 会话标签的词汇与变更原语

"""会话标签（tag）—— 校验与变更的**唯一入口**（纯函数，无 I/O）。

标签是会话级结构化标记（``SessionMetadata.tags``）：**不透明字符串**——
``scheduler`` / ``favorite`` 这样的裸词与 ``task=wing-tag`` 这样的 k=v 约定
都是普通标签，系统不做语义解析（匹配一律精确字符串比较）。约定全小写、
``k=v`` 作命名空间；这些是建议不是语法。

两条边：**写**（``add`` / ``remove``）走 ``validate_tag`` 严格校验，违规即
raise ValueError（错误信息点名违规值——CLI / Agent 靠它纠错，绝不静默清洗）；
**读**（磁盘载入的存量数据 / 投影）走 ``sanitize_tags``：去重 + 丢弃违规格
值，**绝不 raise**（损坏数据不能把读取路径打挂；清洗结果在下次真实写盘时
顺带落盘，自愈）。

约束（violation 即 raise ValueError）：

- 长度 1..64（字节级无关，按字符数）；
- 禁止空白 / 控制字符 / 逗号（逗号是 CLI 参数里的分隔符）；
- 不以 ``-`` 开头（与 CLI 选项区分）；
- 单会话标签数上限 64（``apply_tag_ops`` 在**结果**上校验，只拦扩张）。

变更语义（``apply_tag_ops``）：先加（去重）后删；幂等——重复添加、
移除不存在的标签都不是错误，也不产生变化；``added`` / ``removed`` 报
**净变化**（add 与 remove 同值这类"负负得正"的组合两侧皆空）。
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass, field

MAX_TAG_LENGTH = 64
MAX_TAGS_PER_SESSION = 64


def validate_tag(tag: str) -> str:
    """校验单个标签并原样返回；非法即 raise ValueError。"""
    if not isinstance(tag, str) or not tag:
        raise ValueError(f"invalid tag: {tag!r} (expect a non-empty string)")
    if len(tag) > MAX_TAG_LENGTH:
        raise ValueError(
            f"invalid tag {tag!r}: too long ({len(tag)} > {MAX_TAG_LENGTH} chars)"
        )
    if tag.startswith("-"):
        raise ValueError(f"invalid tag {tag!r}: must not start with '-'")
    for ch in tag:
        if ch.isspace() or ch == "," or ord(ch) < 32 or ord(ch) == 127:
            raise ValueError(
                f"invalid tag {tag!r}: contains forbidden character {ch!r} "
                "(whitespace / comma / control characters are not allowed)"
            )
    return tag


def normalize_tags(tags: Iterable[str]) -> list[str]:
    """逐个校验 + 去重（保持首次出现顺序）。"""
    result: list[str] = []
    for tag in tags:
        validated = validate_tag(tag)
        if validated not in result:
            result.append(validated)
    return result


def sanitize_tags(tags: Iterable[str] | None) -> list[str]:
    """读侧清洗：去重 + 丢弃违规格的值；**绝不 raise**。

    用于磁盘载入的存量数据（可能被手改 / 老版本写入）在投影前归一：损坏
    数据不能把读取路径打挂，非法值只是不投影；下一次真实变更落盘时写入的
    是清洗后的集合（自愈）。
    """
    result: list[str] = []
    for tag in tags or []:
        try:
            validated = validate_tag(tag)
        except ValueError:
            continue
        if validated not in result:
            result.append(validated)
    return result


@dataclass
class TagMutation:
    """一次标签变更的结果。

    - ``tags``：变更后的全量标签（插入序；已有保持原序，新增追加在后）；
    - ``added`` / ``removed``：本次**实际**新增 / 移除（幂等 no-op 不计）。
    """

    tags: list[str] = field(default_factory=list)
    added: list[str] = field(default_factory=list)
    removed: list[str] = field(default_factory=list)


def apply_tag_ops(
    existing: Iterable[str] | None,
    *,
    add: Iterable[str] = (),
    remove: Iterable[str] = (),
) -> TagMutation:
    """在 ``existing`` 上应用 add / remove，返回**净变化**结果。

    ``existing`` 先经 ``sanitize_tags`` 清洗（存量脏数据不 raise、只归一）。
    顺序：先按去重后的 ``add`` 追加，再按 ``remove`` 删除（同一标签同时出现在
    两侧时 **remove 胜出**——移除是修正动作，最后生效）。空 add / remove 即
    纯读取（原样返回）。上限 64 只拦**扩张**：存量已超限（损坏 / 手改）允许
    读取与收缩，不允许再增加。

    ``added`` / ``removed`` 报净变化（相对清洗后的 ``existing``）：
    add 与 remove 同值这类净 no-op 组合两侧皆空——调用方据此跳过写盘。
    """
    base = sanitize_tags(existing)
    work = list(base)

    for tag in normalize_tags(add):
        if tag not in work:
            work.append(tag)
    for tag in normalize_tags(remove):
        if tag in work:
            work.remove(tag)

    if len(work) > MAX_TAGS_PER_SESSION and len(work) > len(base):
        raise ValueError(
            f"too many tags: {len(work)} > {MAX_TAGS_PER_SESSION} "
            "(remove some tags first)"
        )

    added = [tag for tag in work if tag not in base]
    removed = [tag for tag in base if tag not in work]
    return TagMutation(tags=work, added=added, removed=removed)
