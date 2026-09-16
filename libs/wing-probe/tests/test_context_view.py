"""ContextView 规范化与断言行为（tasks 3.7）。

构造请求体 → 断言规范化结果；断言失败必须能定位（消息下标 + call_id）。
另覆盖请求留档的按 model / 序号检索（tasks 3.4）。
"""

import json
from typing import Any, cast

import pytest

from wing_probe.provider.context import (
    ContextAssertionError,
    ContextView,
    MessageView,
    flatten_content,
    match_message,
    normalize_text,
)
from wing_probe.provider.request_log import RequestLog

MODEL = "probe/unit"


def make_body(**overrides: object) -> dict:
    """一个配对完整的请求体（system + user + tool_call + tool + assistant）。"""
    body: dict = {
        "model": MODEL,
        "stream": True,
        "messages": [
            {"role": "system", "content": "you are a probe"},
            {"role": "user", "content": "list files"},
            {
                "role": "assistant",
                "content": "",
                "reasoning_content": "think",
                "tool_calls": [
                    {
                        "id": "call_0_0",
                        "type": "function",
                        "function": {
                            "name": "Bash",
                            "arguments": json.dumps({"command": "ls"}),
                        },
                    }
                ],
            },
            {"role": "tool", "tool_call_id": "call_0_0", "content": "a.txt"},
            {"role": "assistant", "content": "done"},
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "Bash",
                    "description": "run a command",
                    "parameters": {"type": "object", "properties": {}},
                },
            }
        ],
    }
    body.update(overrides)
    return body


def _drop_message(body: dict, index: int) -> dict:
    messages = list(body["messages"])
    del messages[index]
    return {**body, "messages": messages}


# ── 规范化 ──────────────────────────────────────────────────────


def test_system_extracted_and_messages_keep_body_indices() -> None:
    view = ContextView(make_body())
    assert view.system == "you are a probe"
    assert view.role_sequence() == ["user", "assistant", "tool", "assistant"]
    assert [m.index for m in view.messages] == [1, 2, 3, 4]
    assert [m.index for m in view.all_messages] == [0, 1, 2, 3, 4]
    assert view.model == MODEL
    assert view.stream is True


def test_tool_calls_structured_and_tool_call_id() -> None:
    view = ContextView(make_body())
    assistant = view.messages[1]
    assert assistant.tool_calls[0].id == "call_0_0"
    assert assistant.tool_calls[0].name == "Bash"
    assert assistant.tool_calls[0].args == {"command": "ls"}
    assert assistant.tool_calls[0].error is None
    assert assistant.call_ids == ("call_0_0",)
    assert assistant.reasoning_content == "think"
    tool = view.messages[2]
    assert tool.tool_call_id == "call_0_0"
    assert tool.content == "a.txt"


def test_tool_names_from_tools_declaration() -> None:
    view = ContextView(make_body())
    assert view.tool_names == ["Bash"]
    assert ContextView({"messages": []}).tool_names == []


def test_content_blocks_are_flattened_with_cache_control() -> None:
    body = make_body(
        messages=[
            {"role": "system", "content": "sys"},
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "hello",
                        "cache_control": {"type": "ephemeral"},
                    }
                ],
            },
        ]
    )
    view = ContextView(body)
    assert view.messages[0].content == "hello"
    assert flatten_content(None) == ""
    assert flatten_content("plain") == "plain"
    assert flatten_content([{"type": "text", "text": "a"}, "b"]) == "a\nb"
    assert flatten_content(123) == "123"


def test_developer_role_treated_as_system() -> None:
    body = make_body(
        messages=[
            {"role": "developer", "content": "policy"},
            {"role": "user", "content": "hi"},
        ]
    )
    view = ContextView(body)
    assert view.system == "policy"
    assert view.role_sequence() == ["user"]


def test_texts_and_messages_of() -> None:
    view = ContextView(make_body())
    assert view.texts("assistant") == ["", "done"]
    assert [m.index for m in view.messages_of("tool")] == [3]


def test_describe_reports_source_and_call_ids() -> None:
    view = ContextView(make_body(), source="request #0")
    described = view.describe()
    assert "system: 'you are a probe'" in described
    assert "[2] assistant" in described
    assert "call_0_0 Bash" in described
    assert "request #0" in described


# ── tool 配对 ───────────────────────────────────────────────────


def test_tool_pairing_passes_for_complete_body() -> None:
    ContextView(make_body()).assert_tool_pairing()


def test_tool_pairing_reports_missing_pair_with_call_id_and_index() -> None:
    view = ContextView(_drop_message(make_body(), 3), source="request #1")
    with pytest.raises(ContextAssertionError) as excinfo:
        view.assert_tool_pairing()
    report = str(excinfo.value)
    assert "call_0_0" in report
    assert "assistant message[2]" in report
    assert "request #1" in report


def test_tool_pairing_reports_orphan_tool_message() -> None:
    body = make_body(
        messages=[
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "call_9", "content": "orphan"},
        ]
    )
    with pytest.raises(ContextAssertionError) as excinfo:
        ContextView(body).assert_tool_pairing()
    report = str(excinfo.value)
    assert "orphan" in report
    assert "call_9" in report
    assert "tool message[1]" in report


def test_tool_pairing_reports_missing_tool_call_id() -> None:
    body = make_body(
        messages=[
            {"role": "assistant", "content": "x"},
            {"role": "tool", "content": "no id"},
        ]
    )
    with pytest.raises(ContextAssertionError, match="missing tool_call_id"):
        ContextView(body).assert_tool_pairing()


def test_tool_pairing_reports_truncated_arguments() -> None:
    body = make_body(
        messages=[
            {
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_0_0",
                        "type": "function",
                        "function": {"name": "Bash", "arguments": '{"command": "l'},
                    }
                ],
            },
            {"role": "tool", "tool_call_id": "call_0_0", "content": "x"},
        ]
    )
    view = ContextView(body)
    with pytest.raises(ContextAssertionError) as excinfo:
        view.assert_tool_pairing()
    report = str(excinfo.value)
    assert "call_0_0" in report
    assert "not a valid JSON object" in report
    # 放宽 args 校验后，配对本身是完整的
    view.assert_tool_pairing(require_json_args=False)


def test_tool_pairing_reports_duplicate_call_id() -> None:
    body = make_body(
        messages=[
            {
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_x",
                        "type": "function",
                        "function": {"name": "Bash", "arguments": "{}"},
                    },
                    {
                        "id": "call_x",
                        "type": "function",
                        "function": {"name": "Read", "arguments": "{}"},
                    },
                ],
            },
            {"role": "tool", "tool_call_id": "call_x", "content": "x"},
        ]
    )
    with pytest.raises(ContextAssertionError, match="duplicate call_id"):
        ContextView(body).assert_tool_pairing()


def test_tool_pairing_reports_duplicate_tool_message() -> None:
    body = make_body(
        messages=[
            {
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_x",
                        "type": "function",
                        "function": {"name": "Bash", "arguments": "{}"},
                    }
                ],
            },
            {"role": "tool", "tool_call_id": "call_x", "content": "first"},
            {"role": "tool", "tool_call_id": "call_x", "content": "second"},
        ]
    )
    with pytest.raises(ContextAssertionError, match="duplicate"):
        ContextView(body).assert_tool_pairing()


# ── 前缀 / 尾部断言 ─────────────────────────────────────────────


def test_prefix_like_accepts_str_dict_and_message_view() -> None:
    view = ContextView(make_body())
    view.assert_prefix_like(["user: list files"])
    view.assert_prefix_like(
        [
            {"role": "user", "content": "list files"},
            {"role": "assistant", "has_tool_calls": True, "tool_calls": ["Bash"]},
            {"tool_call_id": "call_0_0", "content": "a.txt"},
        ]
    )
    view.assert_prefix_like([MessageView(index=1, role="user", content="list files")])
    view.assert_prefix_like([])


def test_prefix_like_reports_index_and_call_ids() -> None:
    view = ContextView(make_body(), source="request #2")
    with pytest.raises(ContextAssertionError) as excinfo:
        view.assert_prefix_like(
            ["user: list files", {"role": "assistant", "tool_calls": ["Read"]}]
        )
    report = str(excinfo.value)
    assert "message[2]" in report
    assert "call_0_0" in report, "报告要能指出是哪个 call_id 不符"
    assert "request #2" in report


def test_prefix_like_reports_missing_message() -> None:
    view = ContextView(
        {"model": MODEL, "messages": [{"role": "user", "content": "hi"}]}
    )
    with pytest.raises(ContextAssertionError, match="missing"):
        view.assert_prefix_like(["user: hi", "assistant: done"])


def test_tail_from_checks_suffix() -> None:
    view = ContextView(make_body())
    view.assert_tail_from([{"role": "assistant", "content": "done"}])
    view.assert_tail_from(
        [{"role": "tool", "content": "a.txt"}, {"role": "assistant", "content": "done"}]
    )
    view.assert_tail_from(["assistant: done"])
    view.assert_tail_from([])
    with pytest.raises(ContextAssertionError) as excinfo:
        view.assert_tail_from(["user: list files", "assistant: done"])
    assert "tail mismatch" in str(excinfo.value)


def test_tail_from_requires_enough_messages() -> None:
    view = ContextView(
        {"model": MODEL, "messages": [{"role": "user", "content": "hi"}]}
    )
    with pytest.raises(ContextAssertionError) as excinfo:
        view.assert_tail_from(["user: hi", "assistant: done"])
    assert "at least 2 message(s), got 1" in str(excinfo.value)


def test_str_spec_normalizes_whitespace() -> None:
    body = make_body(messages=[{"role": "user", "content": "  hi\r\n"}])
    ContextView(body).assert_prefix_like(["user: hi"])
    assert normalize_text(" a \r\n") == "a"


def test_unknown_spec_keys_raise() -> None:
    view = ContextView(make_body())
    with pytest.raises(ValueError, match="unknown message spec key"):
        view.assert_prefix_like([{"role": "user", "typo": "x"}])
    with pytest.raises(ValueError, match="unknown tool_call spec key"):
        view.assert_prefix_like([{"tool_calls": [{"tool_name": "Bash"}]}])
    with pytest.raises(ValueError, match="unsupported message spec type"):
        bad_spec = cast(Any, 42)
        match_message(view.messages[0], bad_spec)
    with pytest.raises(ValueError, match="must be 'role: content'"):
        view.assert_prefix_like(["list files"])


def test_match_message_tool_call_arguments() -> None:
    view = ContextView(make_body())
    assistant = view.messages[1]
    assert match_message(assistant, {"tool_calls": [{"id": "call_0_0"}]}) is None
    assert (
        match_message(assistant, {"tool_calls": [{"args": {"command": "ls"}}]}) is None
    )
    reason = match_message(assistant, {"tool_calls": [{"args": {"command": "pwd"}}]})
    assert reason is not None and "args differ" in reason


# ── 请求留档检索 ────────────────────────────────────────────────


def test_request_log_retrieval_by_model_and_index() -> None:
    log = RequestLog()
    log.record(make_body(), model=MODEL)
    log.record(make_body(model="probe/other"), model="probe/other")
    log.record(make_body(), model=MODEL)

    assert len(log) == 3
    assert log.count() == 3
    assert log.count(MODEL) == 2
    assert [entry.index for entry in log.by_model(MODEL)] == [0, 2]
    assert log.get(MODEL, 1).index == 2
    assert log.get().index == 0
    assert log.last(MODEL).index == 2
    assert log.last().index == 2
    assert log.get(MODEL, 1).model_index == 1
    assert "probe/unit×2" in log.summary()
    assert [entry.index for entry in log] == [0, 1, 2]


def test_request_log_index_errors_are_readable() -> None:
    log = RequestLog()
    with pytest.raises(IndexError, match="no requests recorded for model 'probe/x'"):
        log.get("probe/x")
    log.record(make_body(), model=MODEL)
    with pytest.raises(IndexError, match="out of range"):
        log.get(MODEL, 3)
    with pytest.raises(IndexError, match="out of range"):
        log.get(MODEL, -1)


def test_logged_request_exposes_body_and_context() -> None:
    log = RequestLog()
    entry = log.record(make_body(), model=MODEL)
    assert entry.stream is True
    assert entry.message_count == 5
    assert entry.body["model"] == MODEL
    assert "request #0" in entry.label()

    view = entry.context()
    assert view.source == entry.label()
    assert view.system == "you are a probe"
    view.assert_tool_pairing()

    log.clear()
    assert len(log) == 0
    assert log.summary() == "0 requests"
