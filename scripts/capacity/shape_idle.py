#!/usr/bin/env python3
"""负载形态：空转 / 历史体量（``idle``）——测**内存底账**。

建 N 个会话后什么都不驱动，只采网关的 RSS / fd 与「相邻档的每会话内存斜率」。配合
``--history-tokens`` 就是内存轴的另一端：同一 N、不同历史体量，RSS 差 / 字节数 = 每字符
历史的常驻成本。

两个刻意的选择：

- **不建客户端连接**（``subscribe=False``）：订阅会钉住会话（产品的逐出条件之一），
  那 reaper 就永远不会动手，窗口里也就观察不到逐出；
- **把逐出阈值压进窗口**：产品默认 ``idle_ttl_seconds`` 1800 s / 扫描 300 s，秒级采样
  窗口里什么都看不到（等于没测）。会话的空闲计时从**建会话**起算，所以阈值取
  ``warmup + dwell/2``：早于窗口结束（`warmup + dwell`）而晚于窗口开始（`warmup`），
  窗口里**应当**发生一次批量逐出。逐出发生与否以公开的 ``/api/session/release`` 探针
  为准（``released=false`` = reaper 已经收走），**不靠** RSS 落差判断——实测进程被清空
  后 RSS 也不回落（分配器不还页），``peak == end`` 既可能是"没逐出"也可能是"逐出了"。
  ``memory`` 后端的会话不允许逐出（产品禁止销毁非持久数据），因此那两个计数写 ``null``。
"""

from __future__ import annotations

from shapes import ShapeArgs, ShapePlan
from wing_probe.provider.script import Script, Turn

NAME = "idle"
MODEL = f"capacity/{NAME}"
KNOBS: frozenset[str] = frozenset({"history_tokens"})

#: 逐出扫描周期（秒）：窗口里至少要扫一次。
SWEEP_INTERVAL_S = 1.0

#: 逐出阈值占采样窗口的比例（配合 warmup 偏移，落在窗口之内）。
TTL_FRACTION = 0.5


def plan(args: ShapeArgs) -> ShapePlan:
    # 空闲计时从建会话起算，所以"窗口内逐出"要求 warmup < ttl ≤ warmup + dwell：
    # 只按 dwell 推会在 warmup 大时提前收走（产物却仍写窗口末的快照）。
    ttl = max(1.0, (args.span_s - args.dwell_s) + args.dwell_s * TTL_FRACTION)
    return ShapePlan(
        # 没有请求会用到这个剧本；model 仍要声明——会话创建要求 model 落在 id 空间里。
        models=((MODEL, Script(Turn.of(text=".", chunk=1))),),
        session_models=tuple([MODEL] * args.sessions),
        tools=("Read",),
        subscribe=False,
        window_mode="hold",
        env_sessions={
            "eviction": {
                "idle_ttl_seconds": round(ttl, 3),
                "sweep_interval_seconds": SWEEP_INTERVAL_S,
            }
        },
        measure_eviction=True,
    )
