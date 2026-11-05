"""tests/test_wake_quiescence.py — ``done`` 的静息判据（``wing/wake/quiescence.py``）。

判据只读两件**已存事实**：会话运行时状态，以及「谁欠谁一次叫醒」的连线（active 且订阅里
有 done 的会话触发器）。这里只钉纯函数的边界；业务路径（押住 / 不扣配额 / ask 立即投递）
在 ``test_wake_service.py::TestSettledDone``。
"""

from __future__ import annotations

from wing.wake.quiescence import will_move_again


def _check(
    statuses: dict[str, str],
    edges: list[tuple[str, str]],
    *,
    session_id: str = "scheduler",
) -> bool:
    return will_move_again(
        session_id, status_of=lambda sid: statuses.get(sid, "inactive"), edges=edges
    )


class TestWillMoveAgain:
    def test_no_edges_means_settled(self) -> None:
        """叶子会话（executor / reviewer）：没有谁欠它一次叫醒。"""
        assert _check({}, []) is False

    def test_a_running_caller_keeps_it_moving(self) -> None:
        assert _check({"executor": "working"}, [("executor", "scheduler")]) is True

    def test_waiting_counts_as_moving(self) -> None:
        """``waiting`` = 等人回答，答了它就会接着跑，所以仍然是"还会动"。"""
        assert _check({"executor": "waiting"}, [("executor", "scheduler")]) is True

    def test_the_chain_is_followed_transitively(self) -> None:
        """执行者 idle、审查者在跑：孙辈的收口会把执行者叫醒，于是调度者也还会动。"""
        statuses = {"executor": "idle", "reviewer": "working"}
        edges = [("executor", "scheduler"), ("reviewer", "executor")]
        assert _check(statuses, edges) is True

    def test_an_idle_chain_without_activity_is_settled(self) -> None:
        """陈旧订阅（上游自己也不会再动）：两跳全 idle → 静息。"""
        statuses = {"executor": "idle", "reviewer": "idle"}
        edges = [("executor", "scheduler"), ("reviewer", "executor")]
        assert _check(statuses, edges) is False

    def test_unloaded_sessions_are_inactive(self) -> None:
        """逐出 / 从未水合的会话不在这张图里作答（``status_of`` 缺省 inactive）。"""
        assert _check({}, [("executor", "scheduler")]) is False

    def test_its_own_status_is_ignored(self) -> None:
        """自己的状态不参与：它刚收口，是否已落成 idle 取决于 emit 与置空的先后。"""
        assert _check({"scheduler": "working"}, []) is False

    def test_a_sibling_tree_does_not_leak_in(self) -> None:
        """只有指向自己的连线才相关：别的树上有人在跑不影响这一层。"""
        statuses = {"stranger": "working"}
        assert _check(statuses, [("stranger", "someone-else")]) is False

    def test_one_running_caller_is_enough(self) -> None:
        statuses = {"busy": "working", "idle": "inactive"}
        edges = [("idle", "scheduler"), ("busy", "scheduler")]
        assert _check(statuses, edges) is True

    def test_a_cycle_terminates(self) -> None:
        """A 订阅 B、B 订阅 A（防环是刻意不做的）：必须收敛，且按"不会再动"处理。"""
        assert _check({}, [("a", "b"), ("b", "a")], session_id="a") is False

    def test_a_cycle_with_activity_still_reports_moving(self) -> None:
        assert (
            _check({"b": "working"}, [("a", "b"), ("b", "a")], session_id="a") is True
        )
