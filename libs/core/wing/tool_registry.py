# wing/tool_registry.py
import inspect
from types import UnionType
from typing import (
    Callable,
    Dict,
    ForwardRef,
    Literal,
    Union,
    get_args,
    get_origin,
    get_type_hints,
)

from wing.schema import Tool, ToolParam


class ToolRegistry:
    def __init__(self):
        self._tools_map: Dict[str, Tool] = {}

    @property
    def tools(self) -> list[Tool]:
        return sorted(self._tools_map.values(), key=lambda x: x.name)

    def get_tool(self, name) -> Tool | None:
        return self._tools_map.get(name)

    def register(
        self,
        name: str | None = None,
        description: str | None = None,
        params: list[ToolParam] | None = None,
        add_purpose: bool = False,
    ):
        def decorator(fn: Callable) -> Callable:
            sig = inspect.signature(fn)
            hints = get_type_hints(fn)
            inject_agent_param: str | None = None
            tool_params = []
            for param_name, param in sig.parameters.items():
                if param_name == "self":
                    continue
                hint = hints.get(param_name)
                if self._is_agent_type(hint):
                    inject_agent_param = param_name
                    continue
                default = None
                if param.default is not inspect.Parameter.empty:
                    default = param.default
                existing = next(
                    (p for p in (params or []) if p.name == param_name), None
                )
                if existing:
                    tool_params.append(existing)
                    continue
                tool_params.append(
                    ToolParam(
                        name=param_name,
                        type=self._type_to_str(hints.get(param_name))[0],
                        items=self._type_to_str(hints.get(param_name))[1],
                        description="",
                        default=default,
                    )
                )

            # 如果启用了 add_purpose，添加 purpose 参数
            if add_purpose:
                purpose_param = ToolParam(
                    name="purpose",
                    type="string",
                    description="简要说明本次工具调用的目的，控制在20个字以内",
                    default="",
                )
                tool_params.append(purpose_param)

            tool = Tool(
                name=name or getattr(fn, "__name__", ""),
                description=description or inspect.getdoc(fn) or "",
                params=tool_params,
                function=fn,
                inject_agent_param=inject_agent_param,
            )
            self._tools_map[tool.name] = tool
            return fn

        return decorator

    def _is_agent_type(self, t) -> bool:
        if t is None:
            return False

        if isinstance(t, str):
            return "WingAgent" in t

        if isinstance(t, ForwardRef):
            return "WingAgent" in t.__forward_arg__

        # Unwrap Optional[WingAgent] / WingAgent | None so tools may declare
        # `agent: WingAgent | None = None` (callers can omit it, e.g. in tests)
        # while still being auto-injected + hidden from the LLM schema.
        origin = get_origin(t)
        if origin is Union or isinstance(t, UnionType):
            return any(self._is_agent_type(a) for a in get_args(t))

        try:
            return t.__name__ == "WingAgent"
        except AttributeError:
            return False

    def _type_to_str(
        self, t: type | None
    ) -> tuple[
        Literal["string", "integer", "number", "boolean", "array", "object"],
        Literal["string", "integer", "number", "boolean", "array", "object"] | None,
    ]:
        """解析类型，返回 (类型字符串, 数组元素类型或None)"""
        if t is None:
            return ("string", None)

        # 处理 Union 类型（包括 Optional[X] = Union[X, None]）
        origin = get_origin(t)
        if origin is Union or isinstance(t, UnionType):
            args = get_args(t)
            # 找到非 None 的类型
            non_none_types = [a for a in args if a is not type(None)]
            if non_none_types:
                return self._type_to_str(non_none_types[0])
            return ("string", None)

        # 处理泛型类型如 list[str], list[int] 等
        if origin is list:
            args = get_args(t)
            if args:
                elem_type = args[0]
                # 元素类型映射
                elem_mapping: dict[
                    type, Literal["string", "integer", "number", "boolean"]
                ] = {
                    str: "string",
                    int: "integer",
                    float: "number",
                    bool: "boolean",
                }
                items = elem_mapping.get(elem_type, "string")
                return ("array", items)
            return ("array", None)

        # 基本类型映射
        mapping: dict[
            type,
            Literal["string", "integer", "number", "boolean", "array", "object"],
        ] = {
            str: "string",
            int: "integer",
            float: "number",
            bool: "boolean",
            list: "array",
            dict: "object",
        }
        result = mapping.get(t, "string")
        return (result, None)


tool_registry = ToolRegistry()
