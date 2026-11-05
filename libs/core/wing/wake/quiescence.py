"""wing/wake/quiescence.py — 「这一层还会不会自己再动」的判据（纯函数，零 I/O）。

``done`` 的语义是**这一层不会再自己动了**，不是「这一轮说完了」。单层编排里两者同义；
多级体系（manager → scheduler → executor）里，中间层一旦「派完活先结束本轮」，两者就分叉：
那一次收口照旧投递，被叫醒的上层只会看到「它刚派完」，而真正的终态因为配额已被消耗，
再没有人叫醒它（见 ``docs/dev/wake.md`` §1.2）。

判据只能建立在**已存事实**上，且要回答「未来还会不会有轮次」，而不是「现在有没有人在跑」：

- ``edges``：``(source, target)``——一条 active、且订阅里有 ``done`` 的会话触发器。
  ``source`` 收口会叫醒 ``target``，于是这张图就是「谁欠谁一次叫醒」的持久账本；
- ``status_of``：会话运行时状态（``working`` / ``waiting`` = 现在就在动）。

从 ``session_id`` 出发沿 ``edges`` 逆向（target → 它的 source）走一遍：只要撞见一个现在在动
的 session，或者一个自己也被别人欠着叫醒的 session（递归成立），它就还会动。

刻意不进判据的三件事：

- **它自己的状态**：调用点在它自己的 ``turn_result`` 之后，那一刻它是否已经落成 ``idle``
  取决于 emit 与 working 置空的先后——那个顺序不该成为契约（见 service 里的说明）；
- **timer 订阅**（``kind=timer``：心跳 / 定时自唤醒）：它不表达「这层还有活没干完」，
  计进来会让一棵带心跳的树永远不静息；
- **没有唤醒线连着的子会话**（例如 ``--no-notify`` 派发的）：它们收口不叫醒任何人，
  于是「这层不会再动」在机制上确实成立——判据如实回答会发生什么，不猜意图。

环（A 订阅 B、B 订阅 A；防环是刻意不做的，见 docs）由 ``visited`` 收敛：绕回来一律按
「不会再动」处理——宁可早一次投递，也不要把唤醒永久挂住。
"""

from __future__ import annotations

from collections.abc import Callable, Iterable

#: 「现在就在动」的运行时状态（``waiting`` = 等人回答，答了就会接着跑）。
MOVING_STATES = frozenset({"working", "waiting"})


def will_move_again(
    session_id: str,
    *,
    status_of: Callable[[str], str],
    edges: Iterable[tuple[str, str]],
) -> bool:
    """``session_id`` 收口之后，还会不会被某条唤醒线叫起来再跑一轮。

    Args:
        session_id: 刚收口的会话（它自己的状态不参与判定，见模块说明）。
        status_of: 会话 id → 运行时状态（未知 / 已逐出按 ``inactive``）。
        edges: ``(source, target)`` 的唤醒连线（只含会送 done 的 active 会话订阅）。

    Returns:
        ``True`` = 还会动（这一次 done 不成立）；``False`` = 已经静息。
    """
    callers: dict[str, list[str]] = {}
    for source, target in edges:
        callers.setdefault(target, []).append(source)

    visited = {session_id}
    stack = list(callers.get(session_id, ()))
    while stack:
        node = stack.pop()
        if node in visited:
            continue
        visited.add(node)
        if status_of(node) in MOVING_STATES:
            return True
        stack.extend(callers.get(node, ()))
    return False


__all__ = ["MOVING_STATES", "will_move_again"]
