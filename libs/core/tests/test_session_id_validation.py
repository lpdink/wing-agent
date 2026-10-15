"""session id 闸门单元测试（校验矩阵）。

闸门口径是"**仅防路径穿越 + 基本卫生**"：id 由后端生成（默认形态
``YYYYMMDD-HHMMSS-8hex``）**或由编排方自带**（Claude Agent SDK 系消费方用
自己的 UUID 建会话 / 续链）。因此这里断言的是**接受面尽可能宽、拒绝面只剩
危险值**：

- 接受：非空、≤128 字符、任意 UTF-8（UUID / 时间戳形态 / 编排方 id / CJK）；
- 拒绝：路径分隔符、``..``、点开头（含 ``.`` / ``..`` 与存储保留的
  ``.media``）、ASCII 控制字符、空串、超长。

为什么"点开头"也要拒：``<sessions root>/.media`` 是 file 后端的媒体池，
id 直接作路径组件——放行点开头就等于把存储基础设施变成可寻址的"会话"
（见 ``test_store_media.py::TestMediaNotASession``）。
"""

from __future__ import annotations

import pytest

from wing.common.utils import (
    SESSION_ID_MAX_BYTES,
    generate_session_id,
    is_valid_session_id,
    validate_session_id,
)

#: 应当接受的 id：真实编排方会用的形态 + 边界。
ACCEPTED = [
    "20250101-000000-abcdef01",  # 后端默认形态
    "3f2b9d1e-6c1a-4f2b-9d3e-1a2b3c4d5e6f",  # Claude Agent SDK 的 UUID
    "8ad3f0b1e5c74f0d9c2a6b3f4e1d0a9c",  # 无连字符的 32 位 hex
    "session-1",
    "team.alpha.run-7",  # 中间的点合法（只有**开头**的点被拒）
    "收藏-会话",
    "a" * SESSION_ID_MAX_BYTES,  # 恰好达字节上限（ASCII：字符数 == 字节数）
    "收" * 42,  # 126 字节（CJK 每字 3 字节）——字节口径下仍合法
]

#: 应当拒绝的 id：全部是"危险或卫生问题"，不是"形态不符"。
REJECTED = [
    "",  # 空串
    "a" * (SESSION_ID_MAX_BYTES + 1),  # 129 字节（超一个字节）
    "a" * 255,  # 255 字节：NAME_MAX 的临界值也不放行（上限更保守）
    "a" * 256,
    "收" * 43,  # 129 字节（43 个 CJK 字符）——字符口径会误放行的那一档
    "收" * 128,  # 384 字节（旧实现放行、Linux 上 mkdir ENAMETOOLONG）
    "/tmp/absolute",
    "x/y",
    "..",
    ".",
    "../escape",
    "a/b/c",
    "..evil",  # 含 `..`
    "a..b",
    ".media",  # 存储保留命名空间（媒体池）
    ".hidden",
    "\\windows\\style",
    "back\\slash",
    "nul\x00char",
    "esc\x1bchar",
    "del\x7fchar",
    "line\nbreak",
    "tab\tchar",
]


class TestSessionIdGate:
    @pytest.mark.parametrize("value", ACCEPTED)
    def test_accepts(self, value: str):
        assert is_valid_session_id(value) is True
        assert validate_session_id(value) == value

    @pytest.mark.parametrize("value", REJECTED)
    def test_rejects(self, value: str):
        assert is_valid_session_id(value) is False
        with pytest.raises(ValueError):
            validate_session_id(value)

    @pytest.mark.parametrize("value", [None, 42, ["a"], b"abc", object()])
    def test_non_string_is_rejected_without_raising(self, value: object):
        """非字符串不抛异常（解析层闸门靠布尔判断，不能因脏输入炸掉）。"""
        assert is_valid_session_id(value) is False

    @pytest.mark.parametrize(
        "value",
        ["\ud800", "\udc00", "a\ud800b", "\ud83d\ude00"[0], "\ud83d"],
    )
    def test_lone_surrogates_are_rejected(self, value: str):
        """孤立代理字符：合法 JSON 但不是合法 UTF-8。

        放行的后果是 create 成功、随后在 ``encode("utf-8")``（落盘）与 HTTP
        响应序列化处炸 500 / 首写 UnicodeEncodeError——闸门必须在这里拒。
        """
        assert is_valid_session_id(value) is False
        with pytest.raises(ValueError):
            validate_session_id(value)

    def test_surrogate_pair_as_real_emoji_is_fine(self):
        """真代理**对**（合法字符串，如 emoji）不受影响：能编码即合法。"""
        assert is_valid_session_id("\U0001f600") is True

    def test_byte_limit_is_bytes_not_characters(self):
        """上限按**字节**：CJK 每字 3 字节，128 字符（=384 字节）必须被拒。"""
        assert len("收" * 42) == 42 and len(("收" * 42).encode()) == 126
        assert len("收" * 43) == 43 and len(("收" * 43).encode()) == 129
        assert is_valid_session_id("收" * 42) is True
        assert is_valid_session_id("收" * 43) is False
        # 字符口径的旧上限在 ASCII 上等价（128 字符 = 128 字节）
        assert is_valid_session_id("a" * 128) is True
        assert is_valid_session_id("a" * 129) is False

    def test_c1_control_chars_are_not_rejected(self):
        """C1（0x80–0x9F）不是 ASCII 控制字符——按闸门口径放行（只拒 0x00–0x1F / 0x7F）。"""
        assert is_valid_session_id("c1\x9bchar") is True

    def test_error_message_names_the_reason(self):
        with pytest.raises(ValueError) as failure:
            validate_session_id("../escape")
        message = str(failure.value)
        assert "invalid session id" in message
        assert "../escape" in message
        assert str(SESSION_ID_MAX_BYTES) in message
        assert "bytes" in message

    def test_error_message_is_ascii_safe_for_surrogates(self):
        """错误文案必须可 JSON 序列化：原始代理字符拼进消息会让 HTTP 错误体再炸一次。"""
        with pytest.raises(ValueError) as failure:
            validate_session_id("\ud800")
        message = str(failure.value)
        assert "\\ud800" in message  # repr 的转义形式
        assert message.encode("utf-8")  # 可编码 = 可回给客户端

    def test_generated_id_always_passes_the_gate(self):
        """生成与校验同源：默认形态恒合法（存量数据零迁移）。"""
        for _ in range(20):
            sid = generate_session_id()
            assert is_valid_session_id(sid)
            assert validate_session_id(sid) == sid
