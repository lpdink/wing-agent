# wing/session.py
"""
wing/session.py — Session 类

Session 是有行为的对象，在构造器中创建 ContextManager 和 WingAgent。
负责单 session 内部操作：metadata 管理、第一条消息自动 title。

设计约束：
  - Session 通过 from_template 类方法创建（统一入口）
  - Session 不反向引用 agent（agent 在 Session 之下）
  - TrackedList 在第一条消息到来时创建 session 目录（由 _ensure_type 触发）
  - Session 在此基础上写入 metadata.json
"""

from __future__ import annotations

import json
import os
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

from wing.common.logger import log
from wing.common.tracked_list import TrackedList
from wing.config import get_config
from wing.context_manager import ContextManager
from wing.openai_provider import OpenAIProvider
from wing.schema import Message

if TYPE_CHECKING:
    from wing.agent import WingAgent
    from wing.agent_template import AgentTemplate


class Session:
    """有行为的 Session 对象。

    通过 from_template 类方法创建，在构造器中创建 ContextManager 和 WingAgent。
    负责 session 内部操作：metadata 管理、第一条消息自动 title。
    """

    _METADATA_FILE = "metadata.json"

    def __init__(
        self,
        session_id: str,
        messages: TrackedList[Message],
        context_manager: ContextManager,
        agent: "WingAgent",
        workspace: str | None = None,
    ) -> None:
        """底层构造器——由 from_template 调用，不建议直接使用。"""
        self._session_id = session_id
        self._messages = messages
        self._sessions_path = get_config().sessions.resolved_path()
        self._template_name: str | None = None

        # 读取已有 metadata（磁盘恢复场景）
        metadata = self._read_metadata()
        self._session_name: str | None = metadata.get("session_name")
        self._session_workspace: str | None = workspace or metadata.get("workspace")
        self._last_interaction: str | None = metadata.get("last_interaction")

        self._context_manager = context_manager
        self._agent = agent

        self._initial_status = self._agent.get_status()

        log.info(f"Session initialized: {session_id}")

    @classmethod
    def from_template(
        cls,
        template: "AgentTemplate",
        session_id: str,
        messages: TrackedList[Message],
        workspace: str | None = None,
    ) -> "Session":
        """从模板构建全新 Session。

        Args:
            template: 解析后的 AgentTemplate
            session_id: Session ID
            messages: TrackedList 消息列表
            workspace: 工作目录
        """
        from wing.agent import WingAgent

        # 创建 ContextManager
        context_manager = ContextManager(
            session_id=session_id,
            messages=messages,
            system_prompt=template.system_prompt,
            compactor=template.compactor,
            skills_patterns=template.skills_patterns,
            rules_patterns=template.rules_patterns,
            workspace=workspace,
        )

        # 创建 WingAgent
        agent = WingAgent(
            model=template.model,
            model_provider=OpenAIProvider(),
            stream=True,
            context_manager=context_manager,
            tools=template.resolved_tools,
        )

        # 将 workspace 注入 agent state 作为 Bash 工具的 cwd
        if workspace:
            agent.state.set("cwd", str(Path(workspace).resolve()))

        session = cls(
            session_id=session_id,
            messages=messages,
            context_manager=context_manager,
            agent=agent,
            workspace=workspace,
        )
        session._template_name = template.name
        return session

    async def switch_template(self, template: "AgentTemplate") -> None:
        """原地替换 agent，保留消息历史。

        在同一个 session 内：
        1. await 旧 agent.shutdown() 清空 inbox 并 cancel worker
        2. 用同一个 TrackedList 构建新 ContextManager
        3. 用新模板创建新 WingAgent

        不产生 NewSessionEvent，SM 和 TUI 不感知 session 变化。
        """
        from wing.agent import WingAgent

        # 1. 干净关闭旧 agent（清空 inbox + cancel worker + await 完成）
        await self._agent.shutdown()

        # 2. 用同一个 TrackedList 构建新 ContextManager
        self._context_manager = ContextManager(
            session_id=self._session_id,
            messages=self._messages,
            system_prompt=template.system_prompt,
            compactor=template.compactor,
            skills_patterns=template.skills_patterns,
            rules_patterns=template.rules_patterns,
            workspace=self._session_workspace,
        )

        # 3. 创建新 Agent
        self._agent = WingAgent(
            model=template.model,
            model_provider=OpenAIProvider(),
            stream=True,
            context_manager=self._context_manager,
            tools=template.resolved_tools,
        )

        cwd = (
            str(Path(self._session_workspace).resolve())
            if self._session_workspace
            else None
        )
        self._agent.state.set("cwd", cwd)

        self._template_name = template.name
        self._initial_status = self._agent.get_status()
        log.info(f"Session {self._session_id}: switched to agent '{template.name}'")

    # ── 暴露属性 ──────────────────────────────────

    @property
    def agent(self) -> "WingAgent":
        return self._agent

    @property
    def template_name(self) -> str | None:
        """当前 session 使用的 agent 模板名称。"""
        return self._template_name

    @property
    def session_id(self) -> str:
        return self._session_id

    @property
    def context_manager(self) -> ContextManager:
        return self._context_manager

    @property
    def initial_status(self) -> dict:
        return self._initial_status

    @property
    def session_name(self) -> str | None:
        """当前 session 名称（来自 metadata.json）。"""
        return self._session_name

    @property
    def session_workspace(self) -> str | None:
        """当前 session 的工作目录（来自 metadata.json 或构造参数）。"""
        return self._session_workspace

    @property
    def last_interaction(self) -> str | None:
        """最后一次用户互动时间（ISO 8601 格式）。"""
        return self._last_interaction

    @property
    def sessions_path(self) -> Path:
        """sessions 存储根路径（来自配置）。"""
        return self._sessions_path

    # ── metadata 管理 ──────────────────────────────

    @property
    def _metadata_path(self) -> Path:
        return self._sessions_path / self._session_id / self._METADATA_FILE

    def _build_metadata(self) -> dict[str, Any]:
        """构建完整的 metadata dict，显式包含所有非 None 字段。"""
        data: dict[str, Any] = {}
        if self._session_name is not None:
            data["session_name"] = self._session_name
        if self._session_workspace is not None:
            data["workspace"] = self._session_workspace
        if self._last_interaction is not None:
            data["last_interaction"] = self._last_interaction
        return data

    def _write_metadata(self) -> None:
        """原子写入 metadata.json。"""
        data = self._build_metadata()
        if not data:
            return
        target = self._metadata_path
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.with_suffix(f".tmp.{os.getpid()}")
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False)
            f.flush()
            os.fsync(f.fileno())
        os.rename(tmp, target)

    def _read_metadata(self) -> dict[str, Any]:
        """读取 metadata.json，文件不存在或解析失败时返回空 dict。"""
        path = self._metadata_path
        if not path.exists():
            return {}
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except Exception:
            return {}

    def set_title(self, title: str) -> None:
        """设置 session 标题并写入 metadata.json。"""
        self._session_name = title
        self._write_metadata()

    def touch_last_interaction(self) -> None:
        """更新最后互动时间并写入 metadata.json。"""
        self._last_interaction = datetime.now().isoformat()
        self._write_metadata()

    # ── 第一条消息 metadata 写入 ──────────────────

    def _check_first_message_metadata(self, content: str) -> None:
        """检查是否是第一条用户消息，如果是则写入 metadata.json。

        在 TrackedList 创建目录前预先创建目录，然后写入 metadata.json。
        TrackedList._assert_initialized 使用 exist_ok=True，预创建目录无影响。
        """
        if self._session_name is not None:
            return  # 已有标题，不是第一条消息
        metadata_path = self._metadata_path
        if metadata_path.exists():
            return  # 已有 metadata（磁盘恢复）
        # 第一条用户消息——写 title
        self._session_name = content[:100]
        # 确保目录存在（TrackedList 后续也会创建，但我们需要先写 metadata）
        metadata_path.parent.mkdir(parents=True, exist_ok=True)
        self._write_metadata()

    async def post(self, content: str, request_id: str | None = None) -> None:
        """投递用户消息。

        先检查并写入第一条消息 metadata，更新最后互动时间，再转发给 agent。
        """
        self._check_first_message_metadata(content)
        self.touch_last_interaction()
        await self._agent.post(content, request_id=request_id)
