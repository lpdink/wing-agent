"""wing/wake/quiescence.py — 「这一层还会不会自己再动」的判据（纯函数，零 I/O）。

``done`` 的语义是**这一层不会再自己动了**，不是「这一轮说完了」。单层编排里两者同义；
多级体系（manager → scheduler → executor）里，中间层一旦「派完活先结束本轮」，两者就分叉：
那一次收口照旧投递，被叫醒的上层只会看到「它刚派完」，而真正的终态因为配额已被消耗，
再没有人叫醒它（见 ``docs/dev/wake.md`` §1.2）。

判据只能建立在**已存事实**上，且要回答「未来还会不会有轮次」，而不是「现在有没有人在跑」：

- ``will_run``：该会话**自己**还会不会再跑一轮——正在跑、在等人类回答、或者 inbox 里已经
  排上了下一轮的输入（``ports.session_will_run_again`` 是它的唯一实现）。``idle`` 且没有
  排队输入 = 这一轮就是它最后的状态。
- ``edges``：``(source, target)``——一条 active、且订阅里有 ``done`` 的会话触发器。
  ``source`` 收口会叫醒 ``target``，于是这张图就是「谁欠谁一次叫醒」的持久账本。

从 ``session_id`` 出发：**它自己**还会跑 → 还会动；否则沿 ``edges`` 逆向（target → 它的
source）走一遍，撞见第一个 ``will_run`` 为真的会话就说明这条链还会把它叫起来。

刻意不进判据的两件事：

- **timer 订阅**（``kind=timer``：心跳 / 定时自唤醒）：它是「到点叫醒」的计划，不是「这一层
  还有活没干完」。计进来会让一棵带心跳的树永远不静息。
- **没有唤醒线连着的子会话**（例如 ``--no-notify`` 派发的）：它们收口不叫醒任何人，
  于是「这层不会再动」在机制上确实成立——判据如实回答会发生什么，不猜意图。

环（A 订阅 B、B 订阅 A；防环是刻意不做的，见 docs）由 ``visited`` 收敛：绕回来一律按
「不会再动」处理——宁可早一次投递，也不要把唤醒永久挂住。
"""

from __future__ import annotations

from collections.abc import Callable, Iterable


def will_move_again(
    session_id: str,
    *,
    will_run: Callable[[str], bool],
    edges: Iterable[tuple[str, str]],
) -> bool:
    """``session_id`` 收口之后，还会不会被叫起来再跑一轮。

    Args:
        session_id: 刚收口的会话。
        will_run: 会话 id → 它自己还会不会再跑一轮（未加载 / 已静默的按 ``False``）。
        edges: ``(source, target)`` 的唤醒连线（只含会送 done 的 active 会话订阅）。

    Returns:
        ``True`` = 还会动（这一次 done 不成立）；``False`` = 已经静息。
    """
    if will_run(session_id):
        return True

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
        if will_run(node):
            return True
        stack.extend(callers.get(node, ()))
    return False


__all__ = ["will_move_again"]
