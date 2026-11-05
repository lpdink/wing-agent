"""tests/test_wake_quiescence.py — ``done`` 的静息判据（``wing/wake/quiescence.py``）。

判据只读两件**已存事实**：该会话自己还会不会再跑一轮（``will_run``），以及「谁欠谁一次
叫醒」的连线（active 且订阅里有 done 的会话触发器）。这里只钉纯函数的边界；业务路径
（押住 / 不扣配额 / ask 立即投递）在 ``test_wake_service.py::TestSettledDone``。
"""

from __future__ import annotations

from wing.wake.quiescence import will_move_again


def _check(
    running: set[str],
    edges: list[tuple[str, str]],
    *,
    session_id: str = "scheduler",
) -> bool:
    return will_move_again(session_id, will_run=lambda sid: sid in running, edges=edges)


class TestWillMoveAgain:
    def test_no_edges_means_settled(self) -> None:
        """叶子会话（executor / reviewer）：没有谁欠它一次叫醒。"""
        assert _check(set(), []) is False

    def test_its_own_next_turn_keeps_it_moving(self) -> None:
        """它自己已经排上下一轮（排队输入 / 已在跑）：这一轮的 done 不成立。"""
        assert _check({"scheduler"}, []) is True

    def test_a_running_caller_keeps_it_moving(self) -> None:
        assert _check({"executor"}, [("executor", "scheduler")]) is True

    def test_the_chain_is_followed_transitively(self) -> None:
        """执行者 idle、审查者在跑：孙辈的收口会把执行者叫醒，于是调度者也还会动。"""
        edges = [("executor", "scheduler"), ("reviewer", "executor")]
        assert _check({"reviewer"}, edges) is True

    def test_an_idle_chain_is_settled(self) -> None:
        """上游自己也不会再动（陈旧 / 部分消耗的订阅）：两跳全静默 → 静息。"""
        edges = [("executor", "scheduler"), ("reviewer", "executor")]
        assert _check(set(), edges) is False

    def test_an_armed_edge_alone_is_not_enough(self) -> None:
        """连线只是"会不会被叫醒"的路径，不是"还会动"的证明——尽头必须有人真在动。"""
        assert _check(set(), [("executor", "scheduler")]) is False

    def test_a_sibling_tree_does_not_leak_in(self) -> None:
        """只有指向自己的连线才相关：别的树上有人在跑不影响这一层。"""
        assert _check({"stranger"}, [("stranger", "someone-else")]) is False

    def test_one_running_caller_is_enough(self) -> None:
        edges = [("idle", "scheduler"), ("busy", "scheduler")]
        assert _check({"busy"}, edges) is True

    def test_a_cycle_terminates(self) -> None:
        """A 订阅 B、B 订阅 A（防环是刻意不做的）：必须收敛，且按"不会再动"处理。"""
        assert _check(set(), [("a", "b"), ("b", "a")], session_id="a") is False

    def test_a_cycle_with_activity_still_reports_moving(self) -> None:
        assert _check({"b"}, [("a", "b"), ("b", "a")], session_id="a") is True
