"""门禁：`TokenCounter.count` 与逐字符参考实现**逐位相等**。

`count` 从 Python 逐字符循环改成 C 层正则扫描（估算要走遍整个上下文窗口，这条路
径在拿不到服务端 usage 时每轮跑 2–3 次）。压缩阈值直接由它的返回值驱动，所以这里
用最笨但最确定的参考实现钉住等价：同一套 CJK 区间、逐字符判定、同一个浮点公式与
同一处 `int()` 取整。

覆盖：空串 / 纯 ASCII / 四个 CJK 区间的边界（区间内第一个与最后一个、区间外紧邻
一个）/ astral 平面（emoji）/ 孤立代理字符 / 混合文本 / 固定种子的随机码点串。
"""

from __future__ import annotations

import random

import pytest

from wing.common.token_counter import TokenCounter

_CJK_RANGES = (
    (0x2E80, 0x2EFF),  # CJK Radicals Supplement
    (0x3400, 0x4DBF),  # CJK Extension A
    (0x4E00, 0x9FFF),  # CJK Unified Ideographs
    (0xF900, 0xFAFF),  # CJK Compatibility Ideographs
)

_BOUNDARIES = (
    0x2E7F,
    0x2E80,
    0x2EFF,
    0x2F00,
    0x33FF,
    0x3400,
    0x4DBF,
    0x4DC0,
    0x4DFF,
    0x4E00,
    0x9FFF,
    0xA000,
    0xF8FF,
    0xF900,
    0xFAFF,
    0xFB00,
    0x0000,
    0x007F,
    0x00E9,
    0x1F600,
    0x1F9D1,
    0x20000,
    0xD800,
    0xDFFF,
)


def _reference_count(text: str) -> int:
    """旧实现（逐字符）的逐字副本。"""
    cjk = 0
    non_cjk = 0
    for ch in text:
        cp = ord(ch)
        if any(low <= cp <= high for low, high in _CJK_RANGES):
            cjk += 1
        else:
            non_cjk += 1
    return int(cjk * 0.7 + non_cjk * 0.3)


def _random_text(rng: random.Random, length: int) -> str:
    chars = []
    for _ in range(length):
        # 覆盖 BMP + astral + 四个 CJK 区间（避开孤立代理，单独用例覆盖）
        bucket = rng.randrange(4)
        if bucket == 0:
            chars.append(chr(rng.randrange(0x20, 0x7F)))
        elif bucket == 1:
            low, high = _CJK_RANGES[rng.randrange(len(_CJK_RANGES))]
            chars.append(chr(rng.randrange(low, high + 1)))
        elif bucket == 2:
            chars.append(chr(rng.randrange(0x1F300, 0x1FAFF)))
        else:
            chars.append(chr(rng.randrange(0x0, 0xD800)))
    return "".join(chars)


CASES = {
    "empty": "",
    "ascii": "hello, world! 12345\n\t{}[]",
    "cjk_unified": "这是一段中文文本，用于估算 token 数量。",
    "mixed": "prefix 中文 suffix 123 ありがとう",
    "emoji": "🙂🚀🎉" * 17,
    "newlines": "\n\r\n\t \x0b\x0c",
    "lone_surrogates": "\ud800\udfff",
    "boundaries": "".join(chr(cp) for cp in _BOUNDARIES),
    "boundary_repeat": "".join(chr(cp) * 7 for cp in _BOUNDARIES),
    "long_ascii": "x" * 200_000,
    "long_cjk": "中" * 100_000,
}


@pytest.mark.parametrize("name", sorted(CASES))
def test_matches_reference(name: str) -> None:
    text = CASES[name]
    assert TokenCounter.count(text) == _reference_count(text)


@pytest.mark.parametrize("seed", range(20))
def test_matches_reference_on_random_text(seed: int) -> None:
    rng = random.Random(seed)
    text = _random_text(rng, 2000)
    assert TokenCounter.count(text) == _reference_count(text)


def test_scanner_sees_both_character_classes() -> None:
    """扫描器自身要有产出：纯 CJK 与纯 ASCII 的估算明显不同（防空转假绿）。"""
    assert TokenCounter.count("中" * 100) > TokenCounter.count("x" * 100)
