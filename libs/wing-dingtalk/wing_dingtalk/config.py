"""配置——全部来自环境变量（compose `.env` 注入）。"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path


def _env(name: str, default: str = "") -> str:
    return os.environ.get(name, default).strip()


def _env_list(name: str) -> list[str]:
    raw = _env(name)
    return [item.strip() for item in raw.split(",") if item.strip()]


@dataclass
class Config:
    """运行时配置（不可变，启动时从环境变量构造）。"""

    # ── 钉钉应用 ──────────────────────────────────────────────
    dingtalk_client_id: str = ""
    dingtalk_client_secret: str = ""
    # 白名单：staff_id 或昵称；空列表 = 放行所有人（启动时打 warning）。
    allowed_users: list[str] = field(default_factory=list)

    # ── Gateway ───────────────────────────────────────────────
    gateway_url: str = "http://127.0.0.1:32523"
    api_key: str | None = None

    # ── 会话 ──────────────────────────────────────────────────
    # 新 session 的 workspace（工具宿主视角的路径）。
    workspace: str = "/workspace/wing-agent"

    # ── 本地状态 / 共享卷 ─────────────────────────────────────
    state_dir: Path = Path("/data")
    # SendFile 工具的可见根（与工具宿主共享的卷挂载点）。
    shared_root: Path = Path("/workspace")

    # ── 身份 ──────────────────────────────────────────────────
    # 事件订阅 WS 的 client_id（admin 角色，收事件）。
    event_client_id: str = "dingtalk-fe"
    # 工具宿主 WS 的 client_id——工具引用形如 `<id>.SendFile`。
    tool_client_id: str = "dingtalk"
    # 开发工具宿主的 client_id（远程六件套的命名空间）。
    tool_host_ns: str = "devbox"
    # 显式会话工具集（覆盖模板 fallback）。None = 按命名空间自动构造。
    session_tools_override: list[str] | None = None
    # 链路就绪前等待注册的工具引用（宿主就位是 create session 的前置条件）。
    expected_tool_refs: list[str] = field(default_factory=list)
    expected_tools_timeout_s: float = 120.0

    # ── 行为 ──────────────────────────────────────────────────
    ack_emoji: str = "👌"
    # 单条钉钉消息最大字符数（超长截断）。
    max_message_chars: int = 18000
    log_level: str = "INFO"

    @classmethod
    def from_env(cls) -> "Config":
        return cls(
            dingtalk_client_id=_env("DINGTALK_CLIENT_ID"),
            dingtalk_client_secret=_env("DINGTALK_CLIENT_SECRET"),
            allowed_users=_env_list("DINGTALK_ALLOWED_USERS"),
            gateway_url=_env("WING_GATEWAY_URL", "http://gateway:32523"),
            api_key=_env("WING_API_KEY") or None,
            workspace=_env("WING_WORKSPACE", "/workspace/wing-agent"),
            state_dir=Path(_env("WING_DINGTALK_STATE_DIR", "/data")),
            shared_root=Path(_env("WING_DINGTALK_SHARED_ROOT", "/workspace")),
            event_client_id=_env("WING_DINGTALK_EVENT_CLIENT_ID", "dingtalk-fe"),
            tool_client_id=_env("WING_DINGTALK_TOOL_CLIENT_ID", "dingtalk"),
            tool_host_ns=_env("WING_DINGTALK_TOOL_HOST_NS", "devbox"),
            session_tools_override=_env_list("WING_SESSION_TOOLS") or None,
            expected_tool_refs=_env_list("WING_EXPECTED_TOOL_REFS"),
            ack_emoji=_env("WING_DINGTALK_ACK_EMOJI", "👌"),
            log_level=_env("WING_DINGTALK_LOG_LEVEL", "INFO"),
        )

    # 远程工具宿主的六个标准工具名。
    _STANDARD_TOOLS = ("Bash", "Read", "Write", "Edit", "Glob", "Grep")
    # TodoWrite 是后端内置工具，重启后模板必然持有它——保证 LLM 的 tools
    # 参数永不为空（部分推理引擎对空 tools 直接不解析工具字段），
    # 使 resume 热切换注入的 System Reminder（含远程工具 schema）可达。
    _ANCHOR_TOOLS = ("TodoWrite",)

    def session_tools(self) -> list[str]:
        """会话显式工具集——create session 时作为 agent override 下发。

        不依赖 gateway 模板 fallback：远程工具须已注册（link 就绪保证），
        此处按命名空间构造引用；WING_SESSION_TOOLS 可整体覆盖。
        """
        if self.session_tools_override:
            return list(self.session_tools_override)
        tools = [f"{self.tool_host_ns}.{name}" for name in self._STANDARD_TOOLS]
        tools.append(f"{self.tool_client_id}.SendFile")
        tools.extend(self._ANCHOR_TOOLS)
        return tools

    def required_tool_refs(self) -> list[str]:
        """链路就绪前必须注册的工具引用（会话工具集的子集即可定位宿主）。"""
        if self.expected_tool_refs:
            return list(self.expected_tool_refs)
        return [f"{self.tool_host_ns}.Bash", f"{self.tool_client_id}.SendFile"]

    def validate(self) -> None:
        """启动前校验——缺失关键配置直接 fail fast。"""
        if not self.dingtalk_client_id or not self.dingtalk_client_secret:
            raise RuntimeError(
                "DINGTALK_CLIENT_ID / DINGTALK_CLIENT_SECRET must be set"
            )
