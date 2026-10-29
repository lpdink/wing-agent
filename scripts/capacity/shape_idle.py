#!/usr/bin/env python3
"""负载形态：空转 / 历史体量（``idle``）——测**内存底账**。

建 N 个会话后什么都不驱动，只采网关的 RSS / fd 与「相邻档的每会话内存斜率」。配合
``--history-tokens`` 就是内存轴的另一端：同一 N、不同历史体量，RSS 差 / 字节数 = 每
字符历史的常驻成本（campaign §2.4）。

两个刻意的选择：

- **不建客户端连接**（``subscribe=False``）：订阅会钉住会话（产品的逐出条件之一），
  那 reaper 就永远不会动手，「dwell 内 RSS 是否回落」也就无从观察；
- **把逐出阈值压进窗口**：产品默认 ``idle_ttl_seconds`` 1800 s / 扫描 300 s，秒级采样
  窗口里什么都看不到（等于没测）。这里写 ``span/2`` 与 1 s，于是窗口里**应当**发生一次
  批量逐出，RSS 的 ``peak`` 与 ``end`` 之差就是它的效果；事后再用公开的
  ``/api/session/release`` 确认「已经被 reaper 收走」（``released=false``）。
  ``memory`` 后端的会话不允许逐出（产品禁止销毁非持久数据），因此那两个计数写 ``null``。
"""

from __future__ import annotations

from shapes import ShapeArgs, ShapePlan, prompt_key
from wing_probe.provider.script import Script, Turn

NAME = "idle"
MODEL = f"capacity/{NAME}"
KNOBS: frozenset[str] = frozenset({"history_tokens"})
WINDOW_MODE = "hold"

#: 逐出扫描周期（秒）：窗口里至少要扫一次。
SWEEP_INTERVAL_S = 1.0

#: 逐出阈值占档位时长的比例（窗口里必然越过）。
TTL_FRACTION = 0.5


def prompt_text(session_id: str, seq: int) -> str:
    """本形态不驱动轮次；契约要求有它（配对键的构造口径与其他形态一致）。"""
    return prompt_key(session_id, seq)


def plan(args: ShapeArgs) -> ShapePlan:
    ttl = max(1.0, args.span_s * TTL_FRACTION)
    return ShapePlan(
        # 没有请求会用到这个剧本；model 仍要声明——会话创建要求 model 落在 id 空间里。
        models=((MODEL, Script(Turn.of(text=".", chunk=1))),),
        session_models=tuple([MODEL] * args.sessions),
        tools=("Read",),
        subscribe=False,
        window_mode=WINDOW_MODE,
        env_sessions={
            "eviction": {
                "idle_ttl_seconds": round(ttl, 3),
                "sweep_interval_seconds": SWEEP_INTERVAL_S,
            }
        },
        measure_eviction=True,
    )
