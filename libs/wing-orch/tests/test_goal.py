"""Goal 状态机单元测试——与 Rust goal.rs 测试 1:1 对应。"""

from wing_orch.goal import (
    DEFAULT_CHECKER_SYSTEM_PROMPT,
    GoalPhase,
    GoalRole,
    GoalState,
    parse_goal_finish,
    parse_reason,
)


def make_goal() -> tuple[GoalState, list]:
    return GoalState.new("exec-session", "create hello.py")


def test_new_produces_initial_actions():
    state, actions = make_goal()
    assert state.phase == GoalPhase.CREATING_CHECKER
    assert state.round == 1
    assert len(actions) == 2
    assert actions[0].kind == "create_checker"
    assert actions[1].kind == "send_executor"


def test_checker_created_transitions():
    state, _ = make_goal()
    actions = state.on_checker_created("checker-session")
    assert actions == []
    assert state.phase == GoalPhase.EXECUTOR_WORKING
    assert state.checker_session_id == "checker-session"


def test_executor_done_sends_to_checker():
    state, _ = make_goal()
    state.on_checker_created("checker-session")
    actions = state.on_turn_result(GoalRole.EXECUTOR, "done")
    assert state.phase == GoalPhase.CHECKER_WORKING
    assert len(actions) == 1
    assert actions[0].kind == "send_checker"
    assert "【执行结果】" in actions[0].content


def test_checker_true_completes():
    state, _ = make_goal()
    state.on_checker_created("checker-session")
    state.on_turn_result(GoalRole.EXECUTOR, "done")
    actions = state.on_turn_result(
        GoalRole.CHECKER,
        "<goal_finish>true</goal_finish>\n<reason>file exists</reason>",
    )
    assert state.phase == GoalPhase.COMPLETED
    assert any(a.kind == "goal_complete" and a.reason == "file exists" for a in actions)
    assert any(a.kind == "exit" for a in actions)


def test_checker_false_loops_back():
    state, _ = make_goal()
    state.on_checker_created("checker-session")
    state.on_turn_result(GoalRole.EXECUTOR, "done")
    actions = state.on_turn_result(
        GoalRole.CHECKER,
        "<goal_finish>false</goal_finish>\n<reason>missing</reason>",
    )
    assert state.phase == GoalPhase.EXECUTOR_WORKING
    assert state.round == 2
    assert any(a.kind == "send_executor" for a in actions)


def test_checker_format_error_retries():
    state, _ = make_goal()
    state.on_checker_created("checker-session")
    state.on_turn_result(GoalRole.EXECUTOR, "done")

    # First format error
    actions = state.on_turn_result(GoalRole.CHECKER, "no tags here")
    assert state.phase == GoalPhase.CHECKER_WORKING
    assert any(a.kind == "send_checker" for a in actions)

    # Second
    state.on_turn_result(GoalRole.CHECKER, "still no tags")

    # Third — exceeds max
    actions = state.on_turn_result(GoalRole.CHECKER, "nope")
    assert state.phase == GoalPhase.INTERRUPTED
    assert any(a.kind == "stall" for a in actions)


def test_interrupted_and_resume():
    state, _ = make_goal()
    state.on_checker_created("checker-session")

    actions = state.on_interrupted(GoalRole.EXECUTOR)
    assert actions == []
    assert state.phase == GoalPhase.INTERRUPTED
    assert state.interrupted_role == GoalRole.EXECUTOR


def test_parse_goal_finish():
    assert parse_goal_finish("<goal_finish>true</goal_finish>") is True
    assert parse_goal_finish("<goal_finish>True</goal_finish>") is True
    assert parse_goal_finish("<goal_finish> TRUE </goal_finish>") is True
    assert parse_goal_finish("<goal_finish>false</goal_finish>") is False
    assert parse_goal_finish("<goal_finish>no</goal_finish>") is False
    assert parse_goal_finish("no tags here") is None
    assert parse_goal_finish("<goal_finish>unclosed") is None


def test_parse_goal_finish_uses_last():
    text = (
        "I should output <goal_finish>true</goal_finish> but first let me verify...\n"
        "After checking, the file is missing.\n"
        "<goal_finish>false</goal_finish>\n<reason>missing</reason>"
    )
    assert parse_goal_finish(text) is False
    assert parse_reason(text) == "missing"


def test_parse_reason():
    assert (
        parse_reason("<reason>file exists and works</reason>")
        == "file exists and works"
    )
    assert parse_reason("no reason tag") is None


def test_serialization_roundtrip():
    state, _ = make_goal()
    state.on_checker_created("checker-id")
    state.appends.append("extra info")
    state.round = 3

    d = state.to_dict()
    restored = GoalState.from_dict(d)
    assert restored.executor_session_id == "exec-session"
    assert restored.checker_session_id == "checker-id"
    assert restored.round == 3
    assert restored.appends == ["extra info"]
    assert restored.phase == GoalPhase.EXECUTOR_WORKING
