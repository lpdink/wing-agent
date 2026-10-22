# wing/store/names.py
"""路径组件的闸门：媒体 id 与上传文件名。

两者都是**路径组件**（防穿越），因此都"拒绝而非清洗"：脏值说明调用方逻辑已经
错了，静默归一（替换 / 截断）只会把 bug 藏起来。``validate_media_content`` 是同
一条「id 是事实」的边界——id 必须与字节一致（内容寻址完整性）。
"""

from __future__ import annotations

import os
import re

from wing.common.utils import InvalidInputError, is_utf8_encodable
from wing.media import media_id as compute_media_id


_MEDIA_ID_PATTERN = re.compile(r"^[0-9a-f]{64}$")


def validate_media_id(media_id: str) -> str:
    """校验 media id 为内容 sha256（64 位小写 hex）并原样返回。

    这是防路径穿越的唯一防线：media id 直接作为存储路径组件，任何其它
    输入都拒绝——不做"清洗"或"容错"（脏 id 说明调用方逻辑已错，静默归一
    只会把 bug 藏起来）。
    """
    if not isinstance(media_id, str) or _MEDIA_ID_PATTERN.fullmatch(media_id) is None:
        raise ValueError(
            f"invalid media id: {media_id!r} (expect 64 lowercase hex chars)"
        )
    return media_id


FILE_NAME_MAX_BYTES = 128
"""上传文件名长度上限（**字节**，不是字符）。

与 session id 同源的理由（``NAME_MAX`` = 255 字节）但更保守：文件名前面还要挂
``stem-2`` 这类冲突后缀，且它只作展示与 agent 识别用，128 字节远超真实文件名。
按字节计 = 中文名合法（``"报告.pdf"`` 是 10 字节），不会在 ``ENAMETOOLONG`` 上炸。
"""


def is_valid_file_name(name: object) -> bool:
    """判断值是否可作为上传文件名（不抛异常，供调用方做闸门判断）。

    规则与 session id 同**精神**不同**实现**（文件名的语义不同：允许 ``-`` 开头、
    允许空格与中文，但仍拒绝点开头——保留存储的点命名空间与"隐藏文件"的歧义）：
    非空、可编码为 UTF-8（孤立代理字符不能）、≤ :data:`FILE_NAME_MAX_BYTES` 字节、
    不含路径分隔符（``/`` ``\\``）或父目录引用（``..``）、不以 ``.`` 开头、
    不含 ASCII 控制字符（0x00–0x1F / 0x7F）。

    拒绝而非清洗：名字是**路径组件**（防穿越），也是一份**用户标识**（agent 通过
    它认得这个文件）——静默归一（替换 / 截断）会让实际落盘名与用户给的名字悄悄
    对不上，违背"响应里的 name 永远是实际落盘名"的契约。
    """
    if not isinstance(name, str) or not name:
        return False
    if not is_utf8_encodable(name):
        return False
    if len(name.encode("utf-8")) > FILE_NAME_MAX_BYTES:
        return False
    if name.startswith("."):
        return False
    if "/" in name or "\\" in name or ".." in name:
        return False
    return not any(ord(ch) < 0x20 or ord(ch) == 0x7F for ch in name)


def validate_file_name(name: str) -> str:
    """校验上传文件名并原样返回；不合规即 ``InvalidInputError``（HTTP 400 语义）。

    错误文案里的名字用 ``repr``（转义形式，恒为 ASCII）：孤立代理字符直接拼进
    消息会在 HTTP 错误体序列化处再次炸（与 ``validate_session_id`` 同款）。
    """
    if not is_valid_file_name(name):
        raise InvalidInputError(
            f"invalid file name: {name!r} "
            f"(expect non-empty UTF-8 encodable name, ≤ {FILE_NAME_MAX_BYTES} bytes, "
            "no path separator / '..' / control char / leading '.')"
        )
    return name


def disambiguated_file_name(name: str, index: int) -> str:
    """同名异字节冲突时的候选名：``stem-<index>.ext``（无扩展名 ``name-<index>``）。

    候选名同样过名字闸门——**加后缀后超长**（原名已顶到字节上限）即
    :class:`InvalidInputError`，文案点明是"冲突后缀把名字顶出上限"而不是调用方
    给的名字本身脏了（原名合法，是"重名 + 超长"这个组合没有合法解）。**不截断**
    stem：静默改名比一次大声失败更糟（那是把用户的标识符悄悄改样）。
    """
    stem, suffix = os.path.splitext(name)
    candidate = f"{stem}-{index}{suffix}"
    if not is_valid_file_name(candidate):
        raise InvalidInputError(
            f"file name too long to disambiguate: {name!r} is already taken by "
            f"different bytes, and the suffixed name {candidate!r} exceeds the "
            f"{FILE_NAME_MAX_BYTES} byte limit"
        )
    return candidate


def validate_media_content(media_id: str, data: bytes) -> None:
    """校验媒体 id 与其字节一致（内容寻址完整性）。

    只在**首写路径**调用（对象已存在时跳过——同 id 必然同内容，跳过省一次
    全量哈希）。不一致说明调用方算错了 id 或传错了 data，必须大声失败：
    否则读回的字节与引用元数据（id/尺寸）永久错位，且没有任何告警。
    """
    actual = compute_media_id(data)
    if actual != media_id:
        raise ValueError(
            f"media content-address mismatch（内容寻址不一致）: "
            f"id={media_id} 与 sha256(data)={actual} 不符"
        )
