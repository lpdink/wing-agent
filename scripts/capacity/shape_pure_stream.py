#!/usr/bin/env python3
"""负载形态：纯长流式（``pure_stream``）——探并发边界的主负载。

每会话一条**持续**的流式回复，帧大小 1 token，帧间隔 ``1/rate`` 秒；一轮结束立刻发
下一条 prompt，因此 N 个会话在整个采样窗口里都在推 delta。网关的每帧成本（序列化 +
一次 task + 一次 socket 写）与「会话数 × 帧率」直接相乘，拐点最先在这里露出来。

刻意不含工具调用、不含历史体量、不含并发突刺——那些是别的形态要隔离的变量。

形态契约（``shapes.resolve`` 校验，新增形态照此实现）：

- ``NAME``：``--scenario`` 的取值；
- ``MODEL``：注册剧本用的 model id（``providers[].models`` 里的 id）；
- ``script(*, sessions, rate, span_s) -> Script``：够整个阶梯档位用的一条剧本；
- ``prompt_text(session_id, seq) -> str``：**全局唯一**的 prompt 文本（配对键）。
"""

from __future__ import annotations

import math

from wing_probe.provider.script import Script, Turn

NAME = "pure_stream"
MODEL = f"capacity/{NAME}"

#: 一「轮」的目标时长上限（秒）。太长 → 窗口内没有完成的轮次，``turn.*`` 无样本；
#: 太短 → 轮次间隙（提交 + 下一次请求组装）占比升高，占空比掉下去。
TURN_TARGET_SECONDS = 5.0

#: 每档期望完成的轮次数：窗口短时按它缩短单轮（否则短窗口一个 ``turn`` 样本都取不到）。
TURNS_PER_SPAN = 5.0


def turn_seconds(span_s: float) -> float:
    """单轮时长（秒）——由档位驱动时长反推，保证窗口内能完成的轮次数量级。"""
    return max(min(TURN_TARGET_SECONDS, span_s / TURNS_PER_SPAN), 0.1)


def turn_tokens(rate: float, span_s: float) -> int:
    """一轮的 token 数（= 帧数，帧大小 1 token）。"""
    return max(1, round(rate * turn_seconds(span_s)))


def prompt_text(session_id: str, seq: int) -> str:
    """第 ``seq`` 轮的用户正文（全局唯一：配对靠它在 ``RequestLog`` 里定位请求）。"""
    return f"load:{session_id}:{seq}"


def script(*, sessions: int, rate: float, span_s: float) -> Script:
    """整档共用的剧本：``sessions × 每会话轮次上界`` 个同款 Turn。

    剧本按 model 注册、由所有会话共享，每次请求消费一个 Turn——因此份额必须覆盖
    「所有会话在整个窗口里能发出的最大请求数」。多给两轮是给轮次间隙与重试的余量；
    一份 Turn 对象被重复引用（frozen dataclass），内存只有一份正文。
    """
    if sessions < 1:
        raise ValueError(f"script() needs sessions >= 1, got {sessions}")
    if rate <= 0:
        raise ValueError(f"script() needs rate > 0, got {rate}")
    tokens = turn_tokens(rate, span_s)
    turns_per_session = math.ceil(span_s / turn_seconds(span_s)) + 2
    turn = Turn.of(text="x" * tokens, chunk=1, delay=1.0 / rate)
    return Script(*([turn] * (sessions * turns_per_session)))


__all__ = [
    "MODEL",
    "NAME",
    "TURN_TARGET_SECONDS",
    "TURNS_PER_SPAN",
    "prompt_text",
    "script",
    "turn_seconds",
    "turn_tokens",
]
