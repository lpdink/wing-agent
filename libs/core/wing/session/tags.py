# wing/session/tags.py — 会话标签的词汇与变更原语

"""会话标签（tag）—— 校验与变更的**唯一入口**（纯函数，无 I/O）。

标签是会话级结构化标记（``SessionMetadata.tags``）：**不透明字符串**——
``scheduler`` / ``favorite`` 这样的裸词与 ``task=wing-tag`` 这样的 k=v 约定
都是普通标签，系统不做语义解析（匹配一律精确字符串比较）。约定全小写、
``k=v`` 作命名空间；这些是建议不是语法。

约束（violation 即 raise ValueError，错误信息点名违规值——CLI / Agent
靠它纠错，绝不静默清洗）：

- 长度 1..64（字节级无关，按字符数）；
- 禁止空白 / 控制字符 / 逗号（逗号是 CLI 参数里的分隔符）；
- 不以 ``-`` 开头（与 CLI 选项区分）；
- 单会话标签数上限 64（``apply_tag_ops`` 在**结果**上校验）。

变更语义（``apply_tag_ops``）：先加（去重）后删；幂等——重复添加、
移除不存在的标签都不是错误，也不产生变化；``added`` / ``removed``
如实报告**实际发生**的增删。
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
    """在 ``existing`` 上原子应用 add / remove，返回变更结果。

    顺序：先按去重后的 ``add`` 追加，再按 ``remove`` 删除（同一标签同时出现在
    两侧时 **remove 胜出**——移除是修正动作，最后生效）。空 add / remove 即
    纯读取（原样返回）。结果超过 ``MAX_TAGS_PER_SESSION`` 即 raise ValueError。
    """
    work = list(existing or [])
    added: list[str] = []
    removed: list[str] = []

    for tag in normalize_tags(add):
        if tag not in work:
            work.append(tag)
            added.append(tag)
    for tag in normalize_tags(remove):
        if tag in work:
            work.remove(tag)
            removed.append(tag)

    if len(work) > MAX_TAGS_PER_SESSION:
        raise ValueError(
            f"too many tags: {len(work)} > {MAX_TAGS_PER_SESSION} "
            "(remove some tags first)"
        )
    return TagMutation(tags=work, added=added, removed=removed)
