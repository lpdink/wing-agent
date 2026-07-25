# tests/test_process_tool_deltas.py — OpenAIProvider._process_tool_deltas 单元测试

"""
测试 _process_tool_deltas 的纯函数行为：
- delta 累积 + partial args 快照
- is_final 时机
- pending.clear() 行为
- 空 id 防御
"""

from types import SimpleNamespace
from unittest.mock import MagicMock

import pytest

from wing.openai_provider import OpenAIProvider
from wing.schema import PendingCall


def _make_choice(tool_calls=None, finish_reason=None):
    """构造模拟 Choice 对象。"""
    delta = SimpleNamespace(tool_calls=tool_calls, content=None)
    # 模拟 reasoning_content 属性不存在
    choice = SimpleNamespace(delta=delta, finish_reason=finish_reason)
    return choice


def _make_tc(index, id=None, name=None, arguments=None):
    """构造模拟 tool_call delta 对象。"""
    function = None
    if name or arguments:
        function = SimpleNamespace(name=name, arguments=arguments)
    return SimpleNamespace(index=index, id=id, function=function)


class TestProcessToolDeltas:
    """_process_tool_deltas 纯函数测试。"""

    def setup_method(self):
        # 创建一个最小的 provider 实例（只需要调用 _process_tool_deltas）
        # 使用 object.__new__ 跳过 __init__（避免需要 config）
        self.provider = object.__new__(OpenAIProvider)

    def test_no_tool_calls_returns_none(self):
        """无 tool_calls 的 chunk 不产出 delta。"""
        pending = {}
        choice = _make_choice(tool_calls=None)
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is None

    def test_empty_tool_calls_list_returns_none(self):
        """空 tool_calls 列表不产出 delta。"""
        pending = {}
        choice = _make_choice(tool_calls=[])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is None

    def test_first_chunk_id_and_name_no_args(self):
        """首个 chunk 只有 id+name，无 args → 不产出 delta（args_buffer 为空）。"""
        pending = {}
        tc = _make_tc(index=0, id="call_1", name="Bash")
        choice = _make_choice(tool_calls=[tc])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is None
        # pending 已注册
        assert 0 in pending
        assert pending[0].id == "call_1"
        assert pending[0].name == "Bash"

    def test_args_delta_produces_partial(self):
        """有 args 碎片时产出 delta + partial_args。"""
        pending = {0: PendingCall(id="call_1", name="Bash", args_buffer='{"com')}
        tc = _make_tc(index=0, arguments='mand": "ls"}')
        choice = _make_choice(tool_calls=[tc])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is not None
        assert len(deltas) == 1
        assert deltas[0].id == "call_1"
        assert deltas[0].name == "Bash"
        assert deltas[0].partial_args == {"command": "ls"}
        assert deltas[0].is_final is False

    def test_finish_reason_produces_finals(self):
        """finish_reason='tool_calls' 时产出 final ToolCall + is_final delta。"""
        pending = {
            0: PendingCall(id="call_1", name="Bash", args_buffer='{"command": "ls"}')
        }
        choice = _make_choice(tool_calls=[], finish_reason="tool_calls")
        # 注意：空 tool_calls 列表 → has_tool_delta=False → 不产出 delta
        # 但 finish_reason 触发 finals
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is not None
        assert len(finals) == 1
        assert finals[0].id == "call_1"
        assert finals[0].arguments == {"command": "ls"}
        # pending 已清空
        assert len(pending) == 0

    def test_finish_with_delta_chunk(self):
        """finish chunk 同时携带 args 碎片 → 同时产出 delta(is_final=True) + finals。"""
        pending = {
            0: PendingCall(id="call_1", name="Read", args_buffer='{"path": "/tmp')
        }
        tc = _make_tc(index=0, arguments='"}')
        choice = _make_choice(tool_calls=[tc], finish_reason="tool_calls")
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is not None
        assert finals[0].arguments == {"path": "/tmp"}
        assert deltas is not None
        assert deltas[0].is_final is True
        assert deltas[0].partial_args == {"path": "/tmp"}
        assert len(pending) == 0

    def test_empty_id_guard(self):
        """id 为空时不产出 delta（防御非标准 provider）。"""
        pending = {0: PendingCall(id="", name="Bash", args_buffer='{"command": "ls"}')}
        tc = _make_tc(index=0, arguments="")  # 有 tool_calls 但无新 args
        # 手动给一个有 arguments 的 tc 触发 has_tool_delta
        tc2 = _make_tc(index=0, arguments=" ")
        choice = _make_choice(tool_calls=[tc2])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        # id 为空 → 不产出 delta
        assert deltas is None

    def test_multiple_tool_calls(self):
        """多个并发 tool call 各自产出 delta。"""
        pending = {
            0: PendingCall(id="call_a", name="Read", args_buffer='{"path": "a.py"'),
            1: PendingCall(id="call_b", name="Bash", args_buffer='{"command": "l'),
        }
        tc0 = _make_tc(index=0, arguments="}")
        tc1 = _make_tc(index=1, arguments='s"}')
        choice = _make_choice(tool_calls=[tc0, tc1])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is not None
        assert len(deltas) == 2
        ids = {d.id for d in deltas}
        assert ids == {"call_a", "call_b"}

    def test_no_redundant_delta_without_tool_chunk(self):
        """pending 非空但当前 chunk 无 tool_calls → 不产出 delta。"""
        pending = {
            0: PendingCall(id="call_1", name="Bash", args_buffer='{"command": "ls"}')
        }
        # 模拟一个只有 content 的 chunk（tool_calls=None）
        choice = _make_choice(tool_calls=None)
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is None
        # pending 不受影响
        assert len(pending) == 1
