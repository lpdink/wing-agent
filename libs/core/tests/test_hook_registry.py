"""HookRegistry 单元测试。

TDD：先定义行为，再实现。

核心行为：
  1. hooks.on(point_name) 注册 handler
  2. handler 按注册顺序执行（管道串联）
  3. handler 签名：(value, **context) → value | None
  4. handler 返回修改后的值 → 替换原始值
  5. handler 返回 None → 保留原始值不修改
  6. context 是常量上下文，整个管道中不变
  7. hook point 是字符串，开放扩展
  8. async handler 支持（invoke_async）
  9. 注册失败（重复注册同一函数到同一 point）应 warning 但不 crash
"""

import pytest

from wing.hook_registry import HookRegistry


# ============================================================
# 同步 handler 测试
# ============================================================


class TestHookRegistrySync:
    def test_register_and_invoke_single_handler(self):
        """单个 handler 修改值"""
        hooks = HookRegistry()
        hooks.on("before_user_message")(lambda msg, **ctx: f"[tagged] {msg}")
        result = hooks.invoke("before_user_message", "hello")
        assert result == "[tagged] hello"

    def test_invoke_with_no_handlers_returns_original(self):
        """无 handler 时返回原始值"""
        hooks = HookRegistry()
        result = hooks.invoke("before_user_message", "hello")
        assert result == "hello"

    def test_handler_returning_none_preserves_original(self):
        """handler 返回 None 表示不修改"""
        hooks = HookRegistry()
        hooks.on("before_user_message")(lambda msg, **ctx: None)
        result = hooks.invoke("before_user_message", "hello")
        assert result == "hello"

    def test_pipeline_chains_handlers_in_registration_order(self):
        """多个 handler 按注册顺序串联执行"""
        hooks = HookRegistry()
        hooks.on("before_user_message")(lambda msg, **ctx: f"A({msg})")
        hooks.on("before_user_message")(lambda msg, **ctx: f"B({msg})")
        result = hooks.invoke("before_user_message", "orig")
        # A 先执行 → "A(orig)"，B 后执行 → "B(A(orig))"
        assert result == "B(A(orig))"

    def test_pipeline_with_none_in_middle_preserves_intermediate(self):
        """管道中间 None 不修改，后续 handler 仍然收到上一个非 None 值"""
        hooks = HookRegistry()
        hooks.on("before_user_message")(lambda msg, **ctx: f"X({msg})")
        hooks.on("before_user_message")(lambda msg, **ctx: None)
        hooks.on("before_user_message")(lambda msg, **ctx: f"Y({msg})")
        result = hooks.invoke("before_user_message", "orig")
        assert result == "Y(X(orig))"

    def test_multiple_hook_points_independent(self):
        """不同 hook point 互不干扰"""
        hooks = HookRegistry()
        hooks.on("before_user_message")(lambda msg, **ctx: f"msg:{msg}")
        hooks.on("after_tool_call")(lambda result, **ctx: f"tool:{result}")
        assert hooks.invoke("before_user_message", "hi") == "msg:hi"
        assert hooks.invoke("after_tool_call", "output") == "tool:output"


# ============================================================
# Context 参数测试（核心新增）
# ============================================================


class TestHookRegistryContext:
    def test_handler_can_access_constant_context(self):
        """handler 通过 **context 接收常量上下文"""
        hooks = HookRegistry()

        def filter_by_tool_name(result: str, **ctx) -> str | None:
            if ctx.get("tool_name") == "Bash":
                return f"[filtered] {result}"
            return None  # 不修改非 Bash 的 result

        hooks.on("after_tool_call")(filter_by_tool_name)
        # Bash tool 的 result 被修改
        assert (
            hooks.invoke("after_tool_call", "output", tool_name="Bash")
            == "[filtered] output"
        )
        # Edit tool 的 result 不修改
        assert hooks.invoke("after_tool_call", "output", tool_name="Edit") == "output"

    def test_context_constant_across_pipeline(self):
        """context 在整个管道中不变，value 被串联修改"""
        hooks = HookRegistry()

        def first(result: str, **ctx) -> str:
            return f"first({result}, {ctx['tool_name']})"

        def second(result: str, **ctx) -> str:
            return f"second({result}, {ctx['tool_name']})"

        hooks.on("after_tool_call")(first)
        hooks.on("after_tool_call")(second)
        result = hooks.invoke("after_tool_call", "orig", tool_name="Edit")
        # first → "first(orig, Edit)"
        # second → "second(first(orig, Edit), Edit)" — ctx 不变
        assert result == "second(first(orig, Edit), Edit)"

    def test_handler_without_context_kwargs(self):
        """handler 不需要接收 **ctx 时，invoke 不传 context"""
        hooks = HookRegistry()
        hooks.on("before_user_message")(lambda msg: f"[tagged] {msg}")
        # 无 context 时正常工作
        result = hooks.invoke("before_user_message", "hello")
        assert result == "[tagged] hello"
        # 有 context 但 handler 不接受 → TypeError 被捕获，跳过 handler
        result = hooks.invoke("before_user_message", "hello", extra="data")
        assert result == "hello"  # handler 被跳过，原始值保留

    def test_handler_accepting_specific_context_keys(self):
        """handler 可以接收特定 context key"""
        hooks = HookRegistry()

        def handler(msg: str, tool_name: str = "") -> str:
            return f"{msg} via {tool_name}"

        hooks.on("before_user_message")(handler)
        result = hooks.invoke("before_user_message", "hello", tool_name="Bash")
        assert result == "hello via Bash"


# ============================================================
# 异步 handler 测试
# ============================================================


class TestHookRegistryAsync:
    @pytest.mark.asyncio
    async def test_async_handler(self):
        """async handler 通过 invoke_async 执行"""
        hooks = HookRegistry()

        async def prefix(msg: str, **ctx) -> str:
            return f"[async] {msg}"

        hooks.on("before_user_message")(prefix)
        result = await hooks.invoke_async("before_user_message", "hello")
        assert result == "[async] hello"

    @pytest.mark.asyncio
    async def test_async_pipeline(self):
        """多个 async handler 串联"""
        hooks = HookRegistry()

        async def first(msg: str, **ctx) -> str:
            return f"A({msg})"

        async def second(msg: str, **ctx) -> str:
            return f"B({msg})"

        hooks.on("before_user_message")(first)
        hooks.on("before_user_message")(second)
        result = await hooks.invoke_async("before_user_message", "orig")
        assert result == "B(A(orig))"

    @pytest.mark.asyncio
    async def test_async_handler_returning_none(self):
        """async handler 返回 None 保留原始值"""
        hooks = HookRegistry()

        async def noop(msg: str, **ctx) -> None:
            return None

        hooks.on("before_user_message")(noop)
        result = await hooks.invoke_async("before_user_message", "hello")
        assert result == "hello"

    @pytest.mark.asyncio
    async def test_async_handler_with_context(self):
        """async handler 通过 **context 接收常量上下文"""
        hooks = HookRegistry()

        async def filter_handler(result: str, **ctx) -> str | None:
            if ctx.get("tool_name") == "Bash":
                return f"[async filtered] {result}"
            return None

        hooks.on("after_tool_call")(filter_handler)
        result = await hooks.invoke_async("after_tool_call", "output", tool_name="Bash")
        assert result == "[async filtered] output"


# ============================================================
# 混合 handler（sync + async）测试
# ============================================================


class TestHookRegistryMixed:
    @pytest.mark.asyncio
    async def test_sync_and_async_handlers_in_pipeline(self):
        """invoke_async 支持 sync handler 混合执行"""
        hooks = HookRegistry()
        hooks.on("before_user_message")(lambda msg, **ctx: f"sync({msg})")

        async def async_handler(msg: str, **ctx) -> str:
            return f"async({msg})"

        hooks.on("before_user_message")(async_handler)
        result = await hooks.invoke_async("before_user_message", "orig")
        assert result == "async(sync(orig))"


# ============================================================
# 边界与注册行为测试
# ============================================================


class TestHookRegistryEdgeCases:
    def test_duplicate_registration_ignored(self):
        """同一函数注册到同一 point 只生效一次"""
        hooks = HookRegistry()

        def once(msg, **ctx):
            return f"once({msg})"

        hooks.on("before_user_message")(once)
        hooks.on("before_user_message")(once)  # 重复注册
        result = hooks.invoke("before_user_message", "orig")
        assert result == "once(orig)"

    def test_handler_error_does_not_crash_pipeline(self):
        """handler 抛异常时跳过该 handler，保留上一个有效值"""
        hooks = HookRegistry()
        hooks.on("before_user_message")(lambda msg, **ctx: f"first({msg})")

        def bad_handler(msg, **ctx):
            raise RuntimeError("boom")

        hooks.on("before_user_message")(bad_handler)
        hooks.on("before_user_message")(lambda msg, **ctx: f"third({msg})")
        # bad_handler 抛异常 → 跳过，保留 "first(orig)"，third 继续处理
        result = hooks.invoke("before_user_message", "orig")
        assert result == "third(first(orig))"

    def test_list_handlers_for_point(self):
        """查询某个 point 的 handler 列表"""
        hooks = HookRegistry()

        def handler1(msg, **ctx):
            return msg

        def handler2(msg, **ctx):
            return msg

        hooks.on("before_user_message")(handler1)
        hooks.on("before_user_message")(handler2)
        assert hooks.handlers("before_user_message") == [handler1, handler2]
        assert hooks.handlers("nonexistent") == []
