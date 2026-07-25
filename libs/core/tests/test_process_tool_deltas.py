# tests/test_process_tool_deltas.py — OpenAIProvider._process_tool_deltas 单元测试

"""
测试 _process_tool_deltas 的纯函数行为（碎片透传语义）：
- args 碎片增量 emit（emitted_len 游标）
- id 到达前缓冲、到达后整体 flush 前缀
- is_final 时机
- pending.clear() 行为
- 空 id 防御
"""

from types import SimpleNamespace

from wing.openai_provider import OpenAIProvider
from wing.schema import PendingCall


def _make_choice(tool_calls=None, finish_reason=None):
    """构造模拟 Choice 对象。"""
    delta = SimpleNamespace(tool_calls=tool_calls, content=None)
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
        """首个 chunk 只有 id+name，无 args → 不产出 delta（无碎片）。"""
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

    def test_args_delta_emits_fragment(self):
        """有 args 碎片时产出 delta，携带原始文本（不解析）。"""
        pending = {
            0: PendingCall(id="call_1", name="Bash", args_buffer='{"com', emitted_len=5)
        }
        tc = _make_tc(index=0, arguments='mand": "ls"}')
        choice = _make_choice(tool_calls=[tc])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is not None
        assert len(deltas) == 1
        assert deltas[0].id == "call_1"
        assert deltas[0].name == "Bash"
        # 只携带本次新增文本
        assert deltas[0].args_fragment == 'mand": "ls"}'
        assert deltas[0].is_final is False
        # 游标前进到 buffer 末尾
        assert pending[0].emitted_len == len(pending[0].args_buffer)

    def test_fragment_is_incremental_across_chunks(self):
        """连续 chunk 的 delta 只含增量，不重复已 emit 的前缀。"""
        pending = {}
        # chunk 1：id + 首段 args → 首个 delta 携带完整前缀
        choice1 = _make_choice(
            tool_calls=[_make_tc(0, id="c1", name="Bash", arguments='{"a": "')]
        )
        _, deltas1 = self.provider._process_tool_deltas(choice1, pending)
        assert deltas1 is not None
        assert deltas1[0].args_fragment == '{"a": "'

        # chunk 2：仅增量
        choice2 = _make_choice(tool_calls=[_make_tc(0, arguments="1")])
        _, deltas2 = self.provider._process_tool_deltas(choice2, pending)
        assert deltas2 is not None
        assert deltas2[0].args_fragment == "1"

        # chunk 3：仅增量
        choice3 = _make_choice(tool_calls=[_make_tc(0, arguments='"}')])
        _, deltas3 = self.provider._process_tool_deltas(choice3, pending)
        assert deltas3 is not None
        assert deltas3[0].args_fragment == '"}'

        # 碎片拼接 = 完整 buffer
        joined = (
            deltas1[0].args_fragment
            + deltas2[0].args_fragment
            + deltas3[0].args_fragment
        )
        assert joined == pending[0].args_buffer == '{"a": "1"}'

    def test_chunk_without_new_args_emits_nothing(self):
        """有 tool_calls 数据但无新 args（重复 chunk）→ 无碎片可 emit。"""
        pending = {
            0: PendingCall(
                id="call_1",
                name="Bash",
                args_buffer='{"command": "ls"}',
                emitted_len=17,
            )
        }
        tc = _make_tc(index=0, name="Bash")  # 只有 name，无 arguments
        choice = _make_choice(tool_calls=[tc])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is None

    def test_finish_reason_produces_finals(self):
        """finish_reason='tool_calls' 时产出 final ToolCall（严格解析）。"""
        pending = {
            0: PendingCall(id="call_1", name="Bash", args_buffer='{"command": "ls"}')
        }
        choice = _make_choice(tool_calls=[], finish_reason="tool_calls")
        # 空 tool_calls 列表 → has_tool_delta=False → 不产出 delta
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is not None
        assert len(finals) == 1
        assert finals[0].id == "call_1"
        assert finals[0].arguments == {"command": "ls"}
        assert deltas is None
        # pending 已清空
        assert len(pending) == 0

    def test_finish_with_delta_chunk(self):
        """finish chunk 同时携带 args 碎片 → 同时产出 delta(is_final=True) + finals。"""
        pending = {
            0: PendingCall(
                id="call_1", name="Read", args_buffer='{"path": "/tmp', emitted_len=14
            )
        }
        tc = _make_tc(index=0, arguments='"}')
        choice = _make_choice(tool_calls=[tc], finish_reason="tool_calls")
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is not None
        assert finals[0].arguments == {"path": "/tmp"}
        assert deltas is not None
        assert deltas[0].is_final is True
        assert deltas[0].args_fragment == '"}'
        assert len(pending) == 0

    def test_args_before_id_flushes_full_prefix(self):
        """非标准 provider：args 先于 id 到达。id 到达前不 emit，
        到达后首个 delta 携带完整前缀（emitted_len 仍为 0）。"""
        pending = {}
        # chunk 1：只有 args，无 id → 缓冲但不 emit
        choice1 = _make_choice(tool_calls=[_make_tc(0, arguments='{"command": ')])
        _, deltas1 = self.provider._process_tool_deltas(choice1, pending)
        assert deltas1 is None
        assert pending[0].emitted_len == 0

        # chunk 2：id 到达 + 新碎片 → emit 完整前缀（含 chunk 1 的内容）
        choice2 = _make_choice(
            tool_calls=[_make_tc(0, id="late_id", name="Bash", arguments='"ls"}')]
        )
        _, deltas2 = self.provider._process_tool_deltas(choice2, pending)
        assert deltas2 is not None
        assert deltas2[0].id == "late_id"
        assert deltas2[0].args_fragment == '{"command": "ls"}'

    def test_empty_id_guard(self):
        """id 始终为空时不产出 delta（防御非标准 provider）。"""
        pending = {0: PendingCall(id="", name="Bash", args_buffer='{"command": "ls"}')}
        tc = _make_tc(index=0, arguments=" ")
        choice = _make_choice(tool_calls=[tc])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is None
        # 游标不前进，id 一旦到达仍可 flush 全量
        assert pending[0].emitted_len == 0

    def test_multiple_tool_calls(self):
        """多个并发 tool call 各自产出增量碎片。"""
        pending = {
            0: PendingCall(id="call_a", name="Read", args_buffer='{"path": "a.py"'),
            1: PendingCall(
                id="call_b", name="Bash", args_buffer='{"command": "l', emitted_len=14
            ),
        }
        tc0 = _make_tc(index=0, arguments="}")
        tc1 = _make_tc(index=1, arguments='s"}')
        choice = _make_choice(tool_calls=[tc0, tc1])
        finals, deltas = self.provider._process_tool_deltas(choice, pending)
        assert finals is None
        assert deltas is not None
        assert len(deltas) == 2
        by_id = {d.id: d for d in deltas}
        # call_a 此前未 emit 过（emitted_len=0）→ 携带完整前缀
        assert by_id["call_a"].args_fragment == '{"path": "a.py"}'
        # call_b 只携带增量
        assert by_id["call_b"].args_fragment == 's"}'

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
