"""轻量 schema 模型——不依赖 wing-gateway。"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class ToolParam:
    """工具参数规格（镜像核心 wing.schema.ToolParam）。"""

    name: str
    type: str = "string"  # string|integer|number|boolean|array|object
    description: str = ""
    default: Any = None
    items: str | None = None  # 数组元素类型

    def to_dict(self) -> dict:
        d: dict[str, Any] = {"name": self.name, "type": self.type}
        if self.description:
            d["description"] = self.description
        if self.default is not None:
            d["default"] = self.default
        if self.items:
            d["items"] = self.items
        return d


@dataclass
class RemoteToolSpec:
    """远程工具规格（注册时用）。"""

    name: str
    description: str = ""
    llm_name: str | None = None
    params: list[ToolParam] = field(default_factory=list)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {"name": self.name}
        if self.description:
            d["description"] = self.description
        if self.llm_name:
            d["llm_name"] = self.llm_name
        if self.params:
            d["params"] = [p.to_dict() for p in self.params]
        return d
