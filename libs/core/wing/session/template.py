# wing/session/template.py
"""Agent 模板系统——多 agent 配置、模板管理。

AgentTemplate: resolved 的模板数据类（从 AgentConfig 解析或从 WingAgent 反向抽取）。
AgentTemplateManager: 从 Config.agents 解析模板列表，提供查询接口。
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from pydantic import BaseModel, ConfigDict, Field

from wing.common.logger import log
from wing.config import AgentConfig, Config
from wing.context import Compactor
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
    provider_name: str
    """绑定的 provider 名称（对应 providers[].name）。由 `agents[].model` 的
    model id 查表得到（`Config.require_model` → `ModelRef.provider_name`）——
    不存在「未指定 = 第一个 provider」的回落。"""
    model_id: str | None = None
    """模型引用词（`agents[].model` 的 effective id）。from_config 恒填；
    from_agent 由调用方传入（fork 传源会话的 id，可空——源会话可能跑在配置
    已删除的 id 上）。模板切换后会话以它作为 `_model_id`。"""
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
    max_turns: int | None = None
    yolo: bool | None = None

    @classmethod
    def from_agent(
        cls,
        agent: "WingAgent",
        name: str | None = None,
        model_id: str | None = None,
    ) -> "AgentTemplate":
        """从已有 WingAgent 反向抽取模板。

        用于 fork/switch 场景下保留当前 agent 配置：``model`` / ``provider_name``
        取自 live agent（运行期事实），``model_id`` 由调用方传入（fork 传源会话的
        引用词；None = 源会话也没有 id）。
        tools 反查 tool_registry 获取未绑定工具。

        ``model_id`` 只是**模板自带 id 的载体**：`Session.from_template` 当前不读
        它（构造期的引用词由恢复链 + identify 兜底决定）。fork 场景里子会话身份的
        **权威来源是 metadata 快照**（`SessionManager.fork_session` 写入的
        ``model_id=source.model_id``），删掉那条快照而指望本参数兜底会静默退化到
        identify 反查——本参数存在的意义是「模板切换 / from_agent 形态下模板携带
        引用词」，不是「fork 身份的保底」。
        """
        cm = agent.context_manager

        unbound_tools = [
            t
            for tool in agent.tools
            if (t := tool_registry.get_tool(tool.name, tool.namespace)) is not None
        ]

        return cls(
            name=name or agent.model,
            model=agent.model,
            provider_name=agent.provider_name,
            model_id=model_id,
            system_prompt=cm.setin_system_prompt,
            resolved_tools=unbound_tools,
            skills_patterns=cm.skills_patterns,
            rules_patterns=cm.rules_patterns,
            compactor=cm.compactor,
            context_window_tokens=cm.compactor.context_window_tokens
            if cm.compactor
            else 100_000,
            max_turns=agent.max_turns,
            yolo=agent.yolo,
        )

    @classmethod
    def from_config(cls, agent_config: AgentConfig, config: Config) -> "AgentTemplate":
        """从 AgentConfig 构建 AgentTemplate。

        ``agents[].model`` 是 model **id**（配置加载期保证 ∈ id 空间）；这里解析成
        运行期二元组：``model`` = 上游调用名、``provider_name`` = 声明该模型的
        provider。解析只有「命中」一种结果——查不中即配置损坏（``require_model``
        抛出 C7 文案）。
        """
        ref = config.require_model(agent_config.model)

        resolved_tools = [
            t
            for name in agent_config.tools
            if (t := tool_registry.resolve(name)) is not None
        ]

        compactor = Compactor(
            context_window_tokens=agent_config.context_window_tokens,
            keep_recent_tokens=agent_config.keep_recent_tokens,
        )

        return cls(
            name=agent_config.name,
            model=ref.name,
            provider_name=ref.provider_name,
            model_id=ref.id,
            system_prompt=agent_config.system_prompt,
            resolved_tools=resolved_tools,
            skills_patterns=list(agent_config.skills),
            rules_patterns=list(agent_config.rules),
            compactor=compactor,
            context_window_tokens=agent_config.context_window_tokens,
            max_turns=agent_config.max_turns,
            yolo=agent_config.yolo,
        )


class AgentTemplateManager:
    """管理 Agent 模板列表。

    从 Config.agents 解析模板，构建 dict[str, AgentTemplate]，提供查询。
    不创建 Session、Agent 或管理生命周期。
    """

    def __init__(self, agents_config: list[AgentConfig], config: Config) -> None:
        """初始化模板管理器。

        Args:
            agents_config: Config.agents 列表（至少一个）
            config: 所属配置——``agents[].model`` 是 model id，必须查表解析成
                (调用名, provider) 二元组（见 `AgentTemplate.from_config`）
        """
        # 解析模板（校验由 Config._validate_config 保证）
        self._templates: dict[str, AgentTemplate] = {}
        for ac in agents_config:
            template = AgentTemplate.from_config(ac, config)
            self._templates[template.name] = template

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
