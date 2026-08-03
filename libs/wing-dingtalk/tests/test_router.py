"""Router 持久化与路由单测。"""

from pathlib import Path

from wing_dingtalk.router import PendingAsk, Router

SAMPLE_MESSAGE = {
    "conversationId": "cid-123",
    "conversationType": "2",
    "senderStaffId": "staff-001",
    "senderNick": "小明",
}


def test_upsert_and_lookup(tmp_path: Path) -> None:
    router = Router(tmp_path / "router.json")
    conv = router.upsert_from_message(SAMPLE_MESSAGE)
    assert conv.conversation_id == "cid-123"
    assert conv.kind == "2"
    assert conv.nick == "小明"
    assert router.get("cid-123") is conv


def test_session_binding_and_reverse_index(tmp_path: Path) -> None:
    router = Router(tmp_path / "router.json")
    conv = router.upsert_from_message(SAMPLE_MESSAGE)
    router.set_session(conv, "sess-abc")
    assert router.conversation_for_session("sess-abc") is conv
    assert router.all_session_ids() == ["sess-abc"]

    # 切换 session 后旧索引失效
    router.set_session(conv, "sess-def")
    assert router.conversation_for_session("sess-abc") is None
    assert router.conversation_for_session("sess-def") is conv


def test_persistence_roundtrip(tmp_path: Path) -> None:
    path = tmp_path / "router.json"
    router = Router(path)
    conv = router.upsert_from_message(SAMPLE_MESSAGE)
    router.set_session(conv, "sess-abc")
    conv.pending_ask = PendingAsk(
        tool_call_id="tc-1",
        session_id="sess-abc",
        questions=[{"id": "q1", "question": "Q1", "choices": ["a", "b"]}],
    )
    router.save()

    loaded = Router(path)
    loaded.load()
    conv2 = loaded.get("cid-123")
    assert conv2 is not None
    assert conv2.session_id == "sess-abc"
    assert conv2.pending_ask is not None
    assert conv2.pending_ask.tool_call_id == "tc-1"
    assert loaded.conversation_for_session("sess-abc") is conv2


def test_pending_ask_progression(tmp_path: Path) -> None:
    ask = PendingAsk(
        tool_call_id="tc",
        session_id="s",
        questions=[
            {"id": "a", "question": "Q1"},
            {"id": "b", "question": "Q2"},
        ],
    )
    first = ask.current()
    assert first is not None
    assert first["id"] == "a"
    assert ask.progress() == "1/2"
    ask.answers.append("ans1")
    ask.idx += 1
    second = ask.current()
    assert second is not None
    assert second["id"] == "b"
    ask.answers.append("ans2")
    ask.idx += 1
    assert ask.current() is None


def test_bound_conversations(tmp_path: Path) -> None:
    router = Router(tmp_path / "router.json")
    conv = router.upsert_from_message(SAMPLE_MESSAGE)
    assert router.bound_conversations() == []
    router.set_session(conv, "s1")
    assert len(router.bound_conversations()) == 1
