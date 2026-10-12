"""工具 schema 的 type hint → LLM 参数类型映射，以及参数注入判定。

`_type_to_str` / `_is_agent_type` 决定工具对 LLM 暴露的参数表（类型错 = 模型传错
参数），另一侧决定哪个参数由运行时注入（`ctx` 不进 schema）。两条都在装饰器
注册路径上，只经注册结果间接验证。
"""

from __future__ import annotations

from typing import Any, ForwardRef, Literal, Optional, Union

import pytest

from wing.agent import ToolContext
from wing.tool_registry import ToolRegistry


def _registry() -> ToolRegistry:
    return ToolRegistry()


# ── _type_to_str ──────────────────────────────────────────────


class TestTypeMapping:
    @pytest.mark.parametrize(
        ("hint", "expected"),
        [
            (None, ("string", None)),
            (str, ("string", None)),
            (int, ("integer", None)),
            (float, ("number", None)),
            (bool, ("boolean", None)),
            (list[str], ("array", "string")),
            (list[int], ("array", "integer")),
            (list[float], ("array", "number")),
            (list[bool], ("array", "boolean")),
            (list[dict], ("array", "string")),  # 未映射元素类型回退 string
            (list, ("array", None)),  # 裸 list：无元素信息
            (dict, ("object", None)),
            # 已知缺口：带参数的映射注解（dict[str, int]）落到统一兜底 string
            # ——它只影响「用映射注解声明参数」的工具（当前内置工具无此用法）。
            (dict[str, Any], ("string", None)),
            (Optional[int], ("integer", None)),
            (int | None, ("integer", None)),
            (str | int | None, ("string", None)),  # 取第一个非 None 分支
            (Literal["a", "b"], ("string", None)),  # 未识别形态的统一兜底
        ],
    )
    def test_mapping(self, hint, expected):
        assert _registry()._type_to_str(hint) == expected


# ── _is_agent_type ────────────────────────────────────────────


class TestAgentParamDetection:
    @pytest.mark.parametrize(
        ("hint", "expected"),
        [
            (None, False),
            ("ToolContext", True),  # 字符串注解（from __future__ import annotations）
            ("WingAgent", True),
            ("SomeOtherType", False),
            (ForwardRef("ToolContext"), True),
            (ForwardRef("Read"), False),
            (int, False),
            (dict, False),
            (Optional["ToolContext"], True),  # Optional 包裹
        ],
    )
    def test_detection(self, hint, expected):
        assert _registry()._is_agent_type(hint) is expected

    def test_object_without_name_is_not_agent(self):
        """无 __name__ 的 typing 构造走 AttributeError 兜底，不能炸注册。"""
        assert _registry()._is_agent_type(Literal["x"]) is False

    def test_union_of_agent_and_scalar_is_injectable(self):
        assert _registry()._is_agent_type(Union[int, "ToolContext"]) is True

    def test_object_without_any_name_attribute(self):
        class _NoName:
            @property
            def __name__(self):  # pragma: no cover - 触发 getattr 异常
                raise AttributeError("no name")

        # 属性访问抛 AttributeError → 兜底为「不是可注入参数」
        assert _registry()._is_agent_type(_NoName()) is False


# ── 装饰器注册：参数表与注入 ──────────────────────────────────


class TestDecoratorParamTable:
    def test_ctx_param_injected_not_exposed(self):
        reg = _registry()

        @reg.register(name="T")
        async def tool(query: str, ctx: ToolContext | None = None) -> str:
            """Doc."""
            return query

        registered = reg.resolve("T")
        assert registered is not None
        assert [p.name for p in registered.params] == ["query"]
        assert registered.inject_agent_param == "ctx"

    def test_method_self_is_skipped(self):
        reg = _registry()

        class Tool:
            @reg.register(name="Method")
            async def run(self, query: str) -> str:
                """Doc."""
                return query

        registered = reg.resolve("Method")
        assert registered is not None
        assert [p.name for p in registered.params] == ["query"]

    def test_explicit_params_win_over_inference(self):
        from wing.schema import ToolParam

        reg = _registry()

        @reg.register(
            name="T",
            params=[ToolParam(name="query", type="string", description="given")],
        )
        async def tool(query: str, extra: int = 3) -> str:
            """Doc."""
            return query

        registered = reg.resolve("T")
        assert registered is not None
        by_name = {p.name: p for p in registered.params}
        assert by_name["query"].description == "given"  # 显式项保留描述
        assert by_name["extra"].type == "integer"  # 未显式给出的照常推断
        assert by_name["extra"].default == 3

    def test_add_description_appends_description_param(self):
        reg = _registry()

        @reg.register(name="T", add_description=True)
        async def tool(query: str) -> str:
            """Doc."""
            return query

        registered = reg.resolve("T")
        assert registered is not None
        assert [p.name for p in registered.params] == ["query", "description"]
        assert registered.params[-1].default == ""

    def test_add_description_defers_to_authored_description_param(self):
        reg = _registry()

        @reg.register(name="T", add_description=True)
        async def tool(title: str, description: str) -> str:
            """Doc."""
            return title

        registered = reg.resolve("T")
        assert registered is not None
        # 工具自己声明的那条留下（无默认值 → required），自动追加的那条让位
        # （default="" 且带固定说明文案）——同名的两条会在 to_openai() 里互相覆盖。
        assert [(p.name, p.default) for p in registered.params] == [
            ("title", None),
            ("description", None),
        ]
        parameters = registered.to_openai()["function"]["parameters"]
        assert parameters["required"] == ["title", "description"]
        assert "default" not in parameters["properties"]["description"]
