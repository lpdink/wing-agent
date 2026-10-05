# wing/config/models.py
# SYNC: Keep this file in sync with default_config.py when adding/removing fields.
import hmac
import os
from pathlib import Path
from typing import Literal

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


class ModelCapabilities(BaseModel):
    """模型能力声明（models 对象形态的 capabilities 字段）。

    未声明 = 全 false（安全默认）；不做名字启发式。frozen：声明是配置事实，
    运行期只读，同时可被 provider 层投影安全共享（无需复制）。
    """

    model_config = ConfigDict(frozen=True, extra="ignore")
    """extra="ignore"：未来新增能力（audio 等）时旧版本忽略未知键而非拒绝配置。"""

    vision: bool = False
    """是否接受图片输入（本期唯一能力）。"""


class ModelSpec(BaseModel):
    """models 列表项的对象形态（新声明协议）。

    字符串形态（存量）等价于只有 name 的 ModelSpec——两种形态经
    ProviderConfig.model_names() / find_model() 统一出口消费。
    """

    model_config = ConfigDict(extra="ignore")
    """extra="ignore"：未来新增字段由新版本消费，旧版本忽略未知键。"""

    name: str
    """实际调用名（传给 provider.generate 的值）。"""
    display_name: str | None = None
    """展示名（人类可读；缺省由前端回落 name）。"""
    description: str | None = None
    capabilities: ModelCapabilities = Field(default_factory=ModelCapabilities)

    @field_validator("name")
    @classmethod
    def _name_non_empty(cls, v: str) -> str:
        if not v.strip():
            raise ValueError("model name must be non-empty")
        return v


class ProviderConfig(BaseModel):
    """单个 LLM provider 配置。"""

    model_config = ConfigDict(extra="ignore")

    name: str
    """用户自定义标识，全局唯一。"""
    protocol: Literal["openai", "anthropic"] = "openai"
    """协议类型。"""
    base_url: str
    api_key: str
    timeout_first_chunk: float = 300.0
    timeout_total: float = 600.0
    max_retries: int = 10
    """LLM 调用失败时的最大重试次数。"""
    max_retry_delay: float = 180.0
    """指数退避最大重试间隔（秒，默认 3 分钟）。"""
    explicit_cache_mode: bool = True
    reasoning_effort: str | None = None
    max_tokens: int = 128_000
    """最大输出 token 数（Anthropic 协议必填）。"""
    extra_body: dict = Field(default_factory=dict)
    """透传到 request body 的额外字段（平铺合并到顶层）。"""
    anthropic_version: str = "2023-06-01"
    """Anthropic API 版本 header（仅 anthropic 协议使用）。"""
    models: list[str | ModelSpec] = Field(default_factory=list)
    """静态模型列表（字符串 = 存量形态；对象 = 带展示元信息与能力声明）。

    配置后不再请求远端 GET /models。消费方用 model_names() / find_model()
    取实际调用名，不要直接翻元素类型。
    """
    image_delivery: Literal["inline", "followup"] | None = None
    """图片投递形态。None = 按协议默认（openai → followup；anthropic → inline）。"""
    image_max_bytes: int | None = None
    """单图请求期兜底上限（字节）；超过的图片位降级为占位文本。None = 不设。"""

    @field_validator("name")
    @classmethod
    def _name_valid(cls, v: str) -> str:
        import re

        if not re.match(r"^[a-zA-Z0-9_-]+$", v):
            raise ValueError(f"provider name must match ^[a-zA-Z0-9_-]+$, got: '{v}'")
        return v

    @field_validator("image_max_bytes")
    @classmethod
    def _image_max_bytes_positive(cls, v: int | None) -> int | None:
        if v is not None and v <= 0:
            raise ValueError(f"image_max_bytes must be > 0 when set, got: {v}")
        return v

    @model_validator(mode="after")
    def _validate_model_declarations(self) -> "ProviderConfig":
        """同一 provider 内实际调用名不得重复（对象/字符串混排也查）。

        重复声明在运行期表现为「同一模型两套能力/展示」，属配置错误，必须在
        解析期报错。
        """
        names = self.model_names()
        if len(names) != len(set(names)):
            seen: set[str] = set()
            for n in names:
                if n in seen:
                    raise ValueError(f"duplicate model name: '{n}'")
                seen.add(n)
        return self

    def model_names(self) -> list[str]:
        """按声明序返回实际调用名（字符串与对象两种形态的统一出口）。"""
        return [m if isinstance(m, str) else m.name for m in self.models]

    def find_model(self, name: str) -> ModelSpec | None:
        """按名查声明；字符串形态包装为等价 ModelSpec，未声明返回 None。"""
        for m in self.models:
            if isinstance(m, str):
                if m == name:
                    return ModelSpec(name=m)
            elif m.name == name:
                return m
        return None


def resolve_model_capabilities(
    provider_cfg: ProviderConfig, model: str
) -> ModelCapabilities:
    """解析模型的能力声明（读图门禁与请求期投影的唯一入口）。

    未声明（含字符串形态 / 不在列表内）= 全 false 的安全默认；不做名字启发式。
    """
    spec = provider_cfg.find_model(model)
    return spec.capabilities if spec is not None else ModelCapabilities()


def resolve_model_display_name(provider_cfg: ProviderConfig, model: str) -> str | None:
    """解析模型的展示名（对前端下发 ``model_display_name`` 的唯一出口）。

    未声明（含字符串形态 / 不在列表内）/ 声明为空串或纯空白 = 无展示名
    （None），前端回落实际调用名。不做名字启发式。
    """
    spec = provider_cfg.find_model(model)
    if spec is None or spec.display_name is None or not spec.display_name.strip():
        return None
    return spec.display_name


class UserAgentConfig(BaseModel):
    preset: Literal["opencode", "qwen-code"] = "qwen-code"


class LogConfig(BaseModel):
    level: str = "WARNING"


class EvictionConfig(BaseModel):
    """空闲会话逐出（eviction）配置。

    逐出只回收内存态（worker / provider client），磁盘状态一概不动：
    被逐出的会话在下一次被需要时按需水合（resume / subscribe / send）。
    钉住条件（不逐出）见 SessionManager._blocked_reason。
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = True
    """总开关。关闭后 reaper 不扫描（调试 / 保守回滚用）。"""

    idle_ttl_seconds: float = Field(default=1800.0, gt=0)
    """空闲时长阈值（秒）：无订阅且超过该时长即逐出。"""

    sweep_interval_seconds: float = Field(default=300.0, gt=0)
    """扫描周期（秒）。启动时读取，热重载不改变已注册 job 的间隔。"""


class SessionsConfig(BaseModel):
    model_config = ConfigDict(extra="ignore")

    eviction: EvictionConfig = Field(default_factory=EvictionConfig)

    def resolved_path(self) -> Path:
        """Session storage path.

        Priority: WING_SESSIONS_PATH env > get_wing_home() / "sessions".
        """
        env_path = os.environ.get("WING_SESSIONS_PATH")
        if env_path:
            return Path(env_path).expanduser()
        # 与 loader 的类型环解环：loader 模块级依赖 models.Config，反向边延迟到调用期。
        from .loader import get_wing_home

        return get_wing_home() / "sessions"


class AgentConfig(BaseModel):
    model_config = ConfigDict(extra="ignore")

    name: str
    model: str
    provider: str | None = None
    """引用的 providers[].name。None 时绑定第一个 provider。"""
    default: bool = False
    system_prompt: str = ""
    tools: list[str] = Field(default_factory=list)
    context_window_tokens: int = 256_000
    keep_recent_tokens: int = 50_000
    skills: list[str] = Field(default_factory=list)
    rules: list[str] = Field(default_factory=list)
    max_turns: int | None = None
    yolo: bool | None = None


class ToolResultTruncateConfig(BaseModel):
    """Tool result truncation policy.

    max_length: trigger threshold in chars. None or <0 disables truncation.
    keep_chars: number of chars to keep at head and tail when truncating.
    """

    max_length: int | None = 100_000
    keep_chars: int = 200

    @field_validator("keep_chars")
    @classmethod
    def _keep_chars_non_negative(cls, v: int) -> int:
        if v < 0:
            raise ValueError("keep_chars must be >= 0")
        return v


class ImagesConfig(BaseModel):
    """图片读入与请求期保留预算配置（read-image）。

    数值全部 > 0：count_quantum / evict_quantum_bytes 是请求期投影算法的
    除数（0 会除零）；高水位为 0 会让所有图被丢，属误配而非「关闭」。
    """

    model_config = ConfigDict(extra="ignore")

    max_bytes: int = Field(default=4_718_592, gt=0)
    """单图原始字节上限（默认 4.5 MiB；ReadImage 读时拒绝 + 降采样提示）。"""
    max_images: int = Field(default=32, gt=0)
    """计数高水位：超出时从最旧开始按 count_quantum 批量驱逐。"""
    count_quantum: int = Field(default=8, gt=0)
    """计数驱逐量子（每次超限批量丢这么多张，KV-cache 友好）。"""
    request_budget_bytes: int = Field(default=37_748_736, gt=0)
    """请求内图片 base64 编码后累计高水位（36 MiB ≈ DeepSeek 48 MiB 的 75%）。"""
    evict_quantum_bytes: int = Field(default=18_874_368, gt=0)
    """字节驱逐量子（= 预算一半，与 DSH 同构）。"""


class ApiKeyEntry(BaseModel):
    """Single API key with an identity role.

    Keys are restricted to ASCII printable characters (0x20–0x7E).
    HTTP headers are latin-1 encoded; non-ASCII keys would silently
    mismatch between client and server.
    """

    key: str
    role: str = "admin"

    @field_validator("key")
    @classmethod
    def _key_ascii_printable(cls, v: str) -> str:
        if not v or not all(0x20 <= ord(c) <= 0x7E for c in v):
            raise ValueError(
                "API key must contain only ASCII printable characters (0x20-0x7E)"
            )
        return v


class AuthConfig(BaseModel):
    """Gateway API key authentication configuration.

    When ``enabled`` is True, all HTTP/WS requests (except exempt paths)
    must carry a valid API key.  ``role`` is enforced (RBAC): ``admin``
    has full access; ``tool_runtime`` may only register remote tools.
    """

    enabled: bool = False
    keys: list[ApiKeyEntry] = Field(default_factory=list)

    def verify(self, key: str) -> str | None:
        """Return the role for *key*, or ``None`` if not found.

        Uses constant-time comparison to prevent timing attacks.
        """
        key_bytes = key.encode("utf-8")
        for entry in self.keys:
            if hmac.compare_digest(key_bytes, entry.key.encode("utf-8")):
                return entry.role
        return None


class GatewayConfig(BaseModel):
    host: str = "127.0.0.1"
    port: int = 32523
    auth: AuthConfig = Field(default_factory=AuthConfig)
    remote_tool_timeout: float = 1800.0
    """远程工具调用总超时（秒）——安全网，非小超时。

    远程工具（如 Bash）可能执行很久，故默认宽口径（30 分钟）。
    断连是首要失败信号（WS 关闭立即 fail 在途调用），此超时仅兜底
    客户端静默挂死但未断连的极端情况。
    """


class CommandsConfig(BaseModel):
    paths: list[str] = Field(default_factory=list)


class Config(BaseModel):
    model_config = ConfigDict(extra="ignore")

    providers: list[ProviderConfig]
    agents: list[AgentConfig]
    hooks: list[str] = Field(default_factory=list)
    safe_command_patterns: list[str] = Field(default_factory=list)
    yolo: bool = False
    steer: bool = True
    log: LogConfig = Field(default_factory=LogConfig)
    sessions: SessionsConfig = Field(default_factory=SessionsConfig)
    commands: CommandsConfig = Field(default_factory=CommandsConfig)
    user_agent: UserAgentConfig = Field(default_factory=UserAgentConfig)
    gateway: GatewayConfig = Field(default_factory=GatewayConfig)
    tool_result_truncate: ToolResultTruncateConfig = Field(
        default_factory=ToolResultTruncateConfig
    )
    images: ImagesConfig = Field(default_factory=ImagesConfig)

    @model_validator(mode="after")
    def _validate_config(self) -> "Config":
        if not self.agents:
            raise ValueError("agents list cannot be empty")
        if not self.providers:
            raise ValueError("providers list cannot be empty")

        # provider name 唯一性
        provider_names = [p.name for p in self.providers]
        if len(provider_names) != len(set(provider_names)):
            seen = set()
            for n in provider_names:
                if n in seen:
                    raise ValueError(f"duplicate provider name: '{n}'")
                seen.add(n)

        # agent name 唯一性
        agent_names = [a.name for a in self.agents]
        if len(agent_names) != len(set(agent_names)):
            seen_agents: set[str] = set()
            for n in agent_names:
                if n in seen_agents:
                    raise ValueError(f"duplicate agent name: '{n}'")
                seen_agents.add(n)

        # agent.provider 引用存在性；未指定时落定第一个 provider（解析阶段
        # 消除可选性——解析产物 AgentTemplate 的 provider_name 为必填）
        provider_name_set = set(provider_names)
        for agent in self.agents:
            if agent.provider is None:
                agent.provider = provider_names[0]
            elif agent.provider not in provider_name_set:
                raise ValueError(
                    f"agent '{agent.name}' references unknown provider '{agent.provider}'"
                )

        return self

    def get_provider(self, name: str | None = None) -> ProviderConfig:
        """获取 provider 配置。name 为 None 时返回第一个。"""
        if name is None:
            return self.providers[0]
        for p in self.providers:
            if p.name == name:
                return p
        raise ValueError(f"provider '{name}' not found")
