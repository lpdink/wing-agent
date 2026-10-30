#!/usr/bin/env python3
"""负载形态：水合风暴（``hydration``）——测**网关重启后 N 个会话一起水合**。

流程：建 N 个会话（可带历史，见 ``--history-tokens``）→ 优雅重启网关 → 全部会话在
同一瞬间订阅。于是三段时间各自成为指标，不混成一个「启动耗时」：

- ``hydration.*`` 订阅请求发出 → ``sync_session`` 到达（读 history.jsonl + 拼快照 + 过 wire）；
- ``first_frame.*`` 驱动 prompt 发出 → 首个正文增量（水合后的第一次真实调用）；
- ``turn.*`` 驱动 prompt 发出 → ``turn_result``（与别的形态同一个口径）。

分片在窗口起点才订阅（``window_mode="hydrate"``）：订阅放在窗口之前的话，尖峰就落在
采样窗口外面了。尖峰的峰值更难采到，所以采样间隔压到 0.25 s（``ps`` / ``lsof`` 每轮
约 17 ms，代价可接受）。

``--backend memory`` 与本形态互斥：进程内会话不过重启，水合必定 404（CLI 直接拒）。
"""

from __future__ import annotations

from shapes import ShapeArgs, ShapePlan
from wing_probe.provider.script import Script, Turn

NAME = "hydration"
MODEL = f"capacity/{NAME}"
KNOBS: frozenset[str] = frozenset({"history_tokens"})

#: 尖峰形态的采样间隔（秒）：1 s 的峰值分辨率会把水合尖峰整个错过。
SAMPLE_INTERVAL_S = 0.25

#: 每会话留给驱动轮的剧本份额（水合后只驱动一轮，余量给重试）。
SCRIPT_TURNS_PER_SESSION = 2


def plan(args: ShapeArgs) -> ShapePlan:
    """驱动轮一次到位：一个事件（``chunk`` 字符）后收口。

    刻意不把驱动轮拉长：``first_frame`` 与 ``turn`` 因此几乎只含「水合之后第一次
    往返」的成本，与 ``pure_stream`` 的稳态流式读数不重叠。
    """
    turn = Turn.of(text="x" * args.chunk, chunk=args.chunk)
    script = Script(*([turn] * (args.sessions * SCRIPT_TURNS_PER_SESSION)))
    return ShapePlan(
        models=((MODEL, script),),
        session_models=tuple([MODEL] * args.sessions),
        tools=("Read",),
        window_mode="hydrate",
        drives_upstream=True,
        # 每个会话只有一轮（水合后的首个 prompt）：有上游连接，没有轮次边界。
        turn_boundaries=False,
        sample_interval_s=SAMPLE_INTERVAL_S,
    )
