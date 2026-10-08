# wing/config/models.py
# SYNC: Keep this file in sync with default_config.py when adding/removing fields.
import hmac
import os
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

_MAX_MODEL_ID_LENGTH = 128
"""显式 model id 的长度上限（name 不设上限，与现状一致）。"""

_UNKNOWN_ID_LIST_LIMIT = 10
"""错误文案里 available ids 的展示上限（超出截断为前 N 个 + ``…``）。"""


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
    ProviderConfig.model_specs() / model_names() 统一出口消费。
    """

    model_config = ConfigDict(extra="ignore")
    """extra="ignore"：未来新增字段由新版本消费，旧版本忽略未知键。"""

    id: str | None = None
    """全局唯一引用词（可选；缺省 = name，见 :attr:`effective_id`）。

    声明后，一切请求 / 协议 / metadata 引用它；``name`` 只作为发给上游的调用名。
    允许 ``:`` ``/`` ``@`` 等可见字符（外部系统的值域不能比我们窄），
    但不接受首尾空白 / 控制字符 / 空串 / 超长——引用词是查表的键，静默改写
    会在两台机器上得到不同的查找结果。
    """
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

    @field_validator("id")
    @classmethod
    def _id_valid(cls, v: str | None) -> str | None:
        if v is None:
            return None
        if v != v.strip():
            raise ValueError(
                f"model id must not have leading/trailing whitespace, got: '{v}'"
            )
        if not v:
            raise ValueError("model id must be non-empty")
        if len(v) > _MAX_MODEL_ID_LENGTH:
            raise ValueError(
                f"model id must be at most {_MAX_MODEL_ID_LENGTH} chars, got {len(v)}"
            )
        if any(ord(c) < 0x20 or ord(c) == 0x7F for c in v):
            raise ValueError(
                f"model id must not contain control characters, got: '{v}'"
            )
        return v

    @property
    def effective_id(self) -> str:
        """生效的引用词：显式 id，缺省回落调用名（存量形态零改动）。"""
        return self.id or self.name


@dataclass
class ModelRef:
    """配置声明的模型引用（解析结果 / 目录条目）。

    ``id`` 是全局唯一引用词（配置加载期强制）；``name`` 是发给上游的调用名；
    ``provider_name`` 是运行期事实维度（不再是引用词）。``spec`` 保留完整声明
    （展示名 / 描述 / 能力）。
    """

    id: str
    name: str
    provider_name: str
    spec: ModelSpec


@dataclass
class ModelGroup:
    """按 provider 分组的目录视图（``Config.model_groups()`` 的元素）。

    分组顺序与组内顺序都是**配置声明序**：目录只有一个来源（配置声明），
    不排序、不回落，配置作者写什么顺序就展示什么顺序。
    """

    provider: str
    models: list[ModelRef]


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
    """静态模型声明（字符串 = 存量形态；对象 = 带展示元信息 / 能力 / 可选 id）。

    这是模型目录的**唯一来源**（远端 ``GET /models`` 发现已退役）；配置加载期
    强制 ``providers[].models`` 非空。消费方用 :meth:`model_specs` /
    :meth:`model_names` 取声明，不要直接翻元素类型；引用词（id）统一经
    ``Config.find_model()`` / ``Config.model_groups()`` 解析。
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

    def model_specs(self) -> list[ModelSpec]:
        """按声明序返回声明项（字符串形态包装为等价 ModelSpec）。"""
        return [
            m if isinstance(m, ModelSpec) else ModelSpec(name=m) for m in self.models
        ]

    def model_names(self) -> list[str]:
        """按声明序返回实际调用名（字符串与对象两种形态的统一出口）。"""
        return [spec.name for spec in self.model_specs()]

    def find_model(self, name: str) -> ModelSpec | None:
        """按**调用名**查声明（运行期口径：agent 持有的是调用名）；未声明返回 None。

        引用词（model id）不走这里——它是全局命名空间，入口是 ``Config.find_model()``。
        """
        for spec in self.model_specs():
            if spec.name == name:
                return spec
        return None


def _model_ref(provider: ProviderConfig, spec: ModelSpec) -> ModelRef:
    """声明项 → 目录条目（id 空间的唯一构造点）。"""
    return ModelRef(
        id=spec.effective_id,
        name=spec.name,
        provider_name=provider.name,
        spec=spec,
    )


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
    """引用的 model id（∈ providers[].models 声明的 id 空间；配置加载期强制）。"""
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

        provider_names = [p.name for p in self.providers]
        if len(provider_names) != len(set(provider_names)):
            seen = set()
            for n in provider_names:
                if n in seen:
                    raise ValueError(f"duplicate provider name: '{n}'")
                seen.add(n)

        agent_names = [a.name for a in self.agents]
        if len(agent_names) != len(set(agent_names)):
            seen_agents: set[str] = set()
            for n in agent_names:
                if n in seen_agents:
                    raise ValueError(f"duplicate agent name: '{n}'")
                seen_agents.add(n)

        # 模型目录 = 配置声明的静态投影（远端 /models 发现已退役）：每个 provider
        # 至少声明一个模型，且 effective id 跨 provider 全局唯一。「全局唯一」是
        # 「解析 = 单键查表」的前提——没有候选集合、没有优先级、没有回落。
        declared_by: dict[str, str] = {}
        for provider in self.providers:
            if not provider.models:
                raise ValueError(
                    f"provider '{provider.name}' declares no models: "
                    "providers[].models must declare at least one model "
                    "(the model catalog comes from configuration only)"
                )
            for spec in provider.model_specs():
                model_id = spec.effective_id
                owner = declared_by.get(model_id)
                if owner is not None:
                    raise ValueError(
                        f"duplicate model id '{model_id}': declared by provider "
                        f"'{owner}' and provider '{provider.name}'.\n"
                        "Give one an explicit id, e.g.:\n"
                        f"  - id: {provider.name}-{spec.name}\n"
                        f"    name: {spec.name}"
                    )
                declared_by[model_id] = provider.name

        # agents[].model 必须落在 id 空间内（未命中即配置错误——错误信息即 C7 文案：
        # available ids + 调用名提示，外部编排方据此一次改对）。
        for agent in self.agents:
            if agent.model not in declared_by:
                raise ValueError(self.describe_unknown_model(agent.model))

        return self

    # ── 模型目录（id 空间）──────────────────────────

    def _iter_model_refs(self) -> Iterator[ModelRef]:
        """按声明序产出全部模型引用（provider 声明序 × 模型声明序）。

        每次调用现扫，不建缓存索引：配置量级是个位数到几十条声明，而现扫对
        构造后的 mutate（测试注入 / 运行期替换）保持同一视图。
        """
        for provider in self.providers:
            for spec in provider.model_specs():
                yield _model_ref(provider, spec)

    def find_model(self, model_id: str) -> ModelRef | None:
        """按 id 查声明——**命中 / 未命中**二值，无候选集合 / 无优先级 / 无回落。

        输入先 trim（id 保证无首尾空白，故 trim 查找仍唯一确定）；未命中返回 None。
        """
        wanted = model_id.strip()
        for ref in self._iter_model_refs():
            if ref.id == wanted:
                return ref
        return None

    def require_model(self, model_id: str) -> ModelRef:
        """按 id 查声明；未命中 raise ValueError，文案见 :meth:`describe_unknown_model`。

        统一 raise 入口：本类（agents[].model 校验）与上层（会话模型切换 / override /
        模板解析）共用同一份文案，外部编排方看到的错误形状稳定。
        """
        ref = self.find_model(model_id)
        if ref is None:
            raise ValueError(self.describe_unknown_model(model_id))
        return ref

    def describe_unknown_model(self, model_id: str) -> str:
        """未命中 id 的自解释错误文案（C7）——纯函数，不含副作用。

        形如::

            unknown model id 'sonnet'; available ids: ds-flash, ds-pro, gpt-4o, …;
            note: 'sonnet' is the call name of model id 'ds-flash' (provider 'local') —
            declare an explicit id or send 'ds-flash'

        available ids 按声明序、超过 10 个截断；请求值命中某声明的**调用名**时给出
        「它就是哪个 id 的调用名」提示。这只是错误路径的提示，**绝不自动生效**
        （不是 resolve——那正是本任务要消灭的东西）。
        """
        ids = [ref.id for ref in self._iter_model_refs()]
        shown = ", ".join(ids[:_UNKNOWN_ID_LIST_LIMIT])
        if len(ids) > _UNKNOWN_ID_LIST_LIMIT:
            shown = f"{shown}, …"
        parts = [f"unknown model id '{model_id}'; available ids: {shown}"]

        wanted = model_id.strip()
        hints = [ref for ref in self._iter_model_refs() if ref.name == wanted]
        if hints:
            noun = "model id" if len(hints) == 1 else "model ids"
            described = " and ".join(
                f"'{ref.id}' (provider '{ref.provider_name}')" for ref in hints
            )
            target = (
                f"send '{hints[0].id}'" if len(hints) == 1 else "send one of those ids"
            )
            parts.append(
                f"note: '{model_id}' is the call name of {noun} {described} — "
                f"declare an explicit id or {target}"
            )
        return "; ".join(parts)

    def identify(self, provider_name: str, name: str) -> str | None:
        """反查 ``(provider, name) → effective id``；未命中返回 None。

        **仅用于旧数据迁移与模板切换补 id**——绝不出现在请求解析路径（请求携带的
        是 id，解析走 :meth:`find_model`）。同 provider 内调用名唯一（ProviderConfig
        校验）保证至多一个命中。
        """
        for ref in self._iter_model_refs():
            if ref.provider_name == provider_name and ref.name == name:
                return ref.id
        return None

    def model_groups(self) -> list[ModelGroup]:
        """模型目录（按 provider 分组，配置声明序；无网络、无 IO）。

        ``/api/models`` 与前端目录的唯一素材来源。
        """
        return [
            ModelGroup(
                provider=provider.name,
                models=[_model_ref(provider, spec) for spec in provider.model_specs()],
            )
            for provider in self.providers
        ]

    def get_provider(self, name: str) -> ProviderConfig:
        """按 name 取 provider 配置（name 必填——不存在「默认 provider」概念）。

        Raises:
            ValueError: name 为空 / 未在配置中声明。
        """
        if not name:
            raise ValueError("provider name is required")
        for p in self.providers:
            if p.name == name:
                return p
        raise ValueError(f"provider '{name}' not found")
