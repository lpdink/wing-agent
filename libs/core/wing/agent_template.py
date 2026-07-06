# wing/agent_template.py
"""Agent 模板系统——多 agent 配置、模板管理。

AgentTemplate: resolved 的模板数据类（从 AgentConfig 解析或从 WingAgent 反向抽取）。
AgentTemplateManager: 从 Config.agents 解析模板列表，提供查询接口。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from pydantic import BaseModel, ConfigDict, Field

from wing.common.logger import log
from wing.config import AgentConfig
from wing.compactor import Compactor
from wing.schema import Tool
from wing.tool_registry import tool_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent


class AgentTemplate(BaseModel):
    """Resolved 的 Agent 模板数据类。

    包含解析后的所有字段：resolved_tools（list[Tool]）、skills/rules patterns 等。
    不创建 Session、Agent 或管理生命周期。
    """

    model_config = ConfigDict(arbitrary_types_allowed=True)

    name: str
    model: str
    system_prompt: str = ""
    resolved_tools: list[Tool] = Field(default_factory=list)
    skills_patterns: list[str] = Field(default_factory=list)
    rules_patterns: list[str] = Field(default_factory=list)
    compactor: Compactor = Field(
        default_factory=lambda: Compactor(
            context_window_tokens=256_000, keep_recent_tokens=50_000
        )
    )
    context_window_tokens: int = 100_000

    @classmethod
    def from_agent(
        cls, agent: "WingAgent", name: str | None = None
    ) -> "AgentTemplate":
        """从已有 WingAgent 反向抽取模板。

        用于 fork/switch 场景下保留当前 agent 配置。
        tools 反查 tool_registry 获取未绑定工具。
        """
        cm = agent.context_manager

        # 反查 tool_registry 获取未绑定工具
        agent_tool_names = [t.name for t in agent.tools]
        unbound_tools = [
            t for n in agent_tool_names if (t := tool_registry.get_tool(n)) is not None
        ]

        return cls(
            name=name or agent.model,
            model=agent.model,
            system_prompt=cm.setin_system_prompt,
            resolved_tools=unbound_tools,
            skills_patterns=list(cm._skills_patterns)
            if hasattr(cm, "_skills_patterns")
            else [],
            rules_patterns=list(cm._rules_patterns)
            if hasattr(cm, "_rules_patterns")
            else [],
            compactor=cm.compactor,
            context_window_tokens=cm.compactor.context_window_tokens
            if cm.compactor
            else 100_000,
        )

    @classmethod
    def from_config(cls, agent_config: AgentConfig) -> "AgentTemplate":
        """从 AgentConfig 构建 AgentTemplate。"""
        # 解析 tools（walrus 避免双重查询）
        resolved_tools = [
            t
            for name in agent_config.tools
            if (t := tool_registry.get_tool(name)) is not None
        ]

        compactor = Compactor(
            context_window_tokens=agent_config.context_window_tokens,
            keep_recent_tokens=agent_config.keep_recent_tokens,
        )

        return cls(
            name=agent_config.name,
            model=agent_config.model,
            system_prompt=agent_config.system_prompt,
            resolved_tools=resolved_tools,
            skills_patterns=list(agent_config.skills),
            rules_patterns=list(agent_config.rules),
            compactor=compactor,
            context_window_tokens=agent_config.context_window_tokens,
        )


class AgentTemplateManager:
    """管理 Agent 模板列表。

    从 Config.agents 解析模板，构建 dict[str, AgentTemplate]，提供查询。
    不创建 Session、Agent 或管理生命周期。
    """

    def __init__(self, agents_config: list[AgentConfig]) -> None:
        """初始化模板管理器。

        Args:
            agents_config: Config.agents 列表（至少一个）
        """
        # 解析模板（校验由 Config._validate_agents 保证）
        self._templates: dict[str, AgentTemplate] = {}
        for ac in agents_config:
            template = AgentTemplate.from_config(ac)
            self._templates[template.name] = template

        # 解析 default
        explicit_default = next((ac.name for ac in agents_config if ac.default), None)

        self._default_name = explicit_default or agents_config[0].name
        log.info(
            f"AgentTemplateManager initialized: {len(self._templates)} templates, "
            f"default='{self._default_name}'"
        )

    def get(self, name: str) -> AgentTemplate | None:
        """按名称查询模板，不存在返回 None。"""
        return self._templates.get(name)

    @property
    def default(self) -> AgentTemplate:
        """返回默认模板。"""
        return self._templates[self._default_name]

    @property
    def default_name(self) -> str:
        """默认模板名称。"""
        return self._default_name

    @property
    def all_names(self) -> list[str]:
        """所有模板名称列表（保持配置顺序）。"""
        return list(self._templates.keys())

    def __contains__(self, name: str) -> bool:
        return name in self._templates

    def __len__(self) -> int:
        return len(self._templates)
