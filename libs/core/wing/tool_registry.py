# wing/tool_registry.py
import inspect
from dataclasses import dataclass
from types import UnionType
from typing import (
    Any,
    Callable,
    ForwardRef,
    Literal,
    Union,
    get_args,
    get_origin,
    get_type_hints,
)

from wing.schema import Tool, ToolParam

DEFAULT_NAMESPACE = "default"


@dataclass(frozen=True)
class ToolRef:
    """工具引用——将字符串引用解析为 (namespace, name) 对。

    解析规则（k8s 风格，rsplit(".", 1)）：
      - "Bash"          → (default, Bash)
      - "client.Bash"   → (client, Bash)
      - "org.team.Bash" → (org.team, Bash)

    约束：default 命名空间内工具名不含 "."。
    """

    namespace: str
    name: str

    @classmethod
    def parse(cls, ref: str) -> "ToolRef":
        if not ref:
            raise ValueError("Tool reference cannot be empty")
        if "." in ref:
            ns, name = ref.rsplit(".", 1)
            if not ns or not name:
                raise ValueError(f"Invalid tool reference: '{ref}'")
            return cls(namespace=ns, name=name)
        return cls(namespace=DEFAULT_NAMESPACE, name=ref)

    def __str__(self) -> str:
        if self.namespace == DEFAULT_NAMESPACE:
            return self.name
        return f"{self.namespace}.{self.name}"


class ToolRegistry:
    def __init__(self) -> None:
        self._namespaces: dict[str, dict[str, Tool]] = {}

    @property
    def tools(self) -> list[Tool]:
        all_tools: list[Tool] = []
        for ns_tools in self._namespaces.values():
            all_tools.extend(ns_tools.values())
        return sorted(all_tools, key=lambda x: (x.namespace, x.name))

    def get_tool(self, name: str, namespace: str = DEFAULT_NAMESPACE) -> Tool | None:
        return self._namespaces.get(namespace, {}).get(name)

    def resolve(self, ref: str) -> Tool | None:
        """解析工具引用字符串（裸名或 namespace.name），返回对应 Tool。

        畸形引用（空串、".Bash" 等）视为"未找到"返回 None，
        不向调用方抛异常——config 解析和 replace_tools 依赖此契约。
        """
        try:
            parsed = ToolRef.parse(ref)
        except ValueError:
            return None
        return self.get_tool(parsed.name, parsed.namespace)

    def register_tool(self, tool: Tool) -> None:
        """注册一个完全构造好的 Tool——外部工具入口。

        与 register() 装饰器不同：不内省函数签名，直接接纳调用方提供的
        schema 与可执行体。Gateway 借此注入远程工具——核心对其 function 的
        来源（进程内 / 网络）不做任何假设，保持网络无关。

        碰撞契约与 register() 一致：同 namespace 同名抛 ValueError。
        """
        ns_map = self._namespaces.setdefault(tool.namespace, {})
        if tool.name in ns_map:
            raise ValueError(
                f"Tool '{tool.name}' already registered in namespace '{tool.namespace}'"
            )
        ns_map[tool.name] = tool

    def unregister_namespace(self, namespace: str) -> list[Tool]:
        """移除并返回某 namespace 下的全部工具。

        用于 tool host 断连时一次性清除其远程工具。namespace 不存在返回空列表。
        """
        ns_map = self._namespaces.pop(namespace, None)
        if not ns_map:
            return []
        return list(ns_map.values())

    def register(
        self,
        name: str | None = None,
        description: str | None = None,
        params: list[ToolParam] | None = None,
        add_purpose: bool = False,
        namespace: str = DEFAULT_NAMESPACE,
        llm_name: str | None = None,
    ) -> Callable[[Callable], Callable]:
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
                namespace=namespace,
                llm_name=llm_name,
                description=description or inspect.getdoc(fn) or "",
                params=tool_params,
                function=fn,
                inject_agent_param=inject_agent_param,
            )
            ns_map = self._namespaces.setdefault(namespace, {})
            if tool.name in ns_map:
                raise ValueError(
                    f"Tool '{tool.name}' already registered in namespace '{namespace}'"
                )
            ns_map[tool.name] = tool
            return fn

        return decorator

    def _is_agent_type(self, t: Any) -> bool:
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
