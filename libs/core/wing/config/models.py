# wing/config/models.py
"""配置模型（pydantic）+ 模型目录（id 空间）/ 能力 / 展示名解析。

字段的元信息（说明 / 枚举含义 / 密文 / 生效域 / 分组）一律经 ``S(...)`` 在**声明处**
声明一次（``config/spec.py``），字段 docstring 不再存在——两边都留就是新的 SYNC。
跨字段检查住在 ``config/problems.py``，本文件的 ``model_validator`` 只是薄适配器
（加载期仍只 raise 第一个 problem，文案逐字不变）。
"""

import hmac
import os
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from pydantic import BaseModel, ConfigDict, field_validator, model_validator

from .problems import (
    cross_field_problems,
    iter_model_refs,
    model_name_message,
    provider_model_problems,
    unknown_model_message,
)
from .spec import ApplyScope, S

_MAX_MODEL_ID_LENGTH = 128
"""显式 model id 的长度上限（name 不设上限，与现状一致）。"""


class ModelCapabilities(BaseModel):
    """模型能力声明（models 对象形态的 capabilities 字段）。

    未声明 = 全 false（安全默认）；不做名字启发式。frozen：声明是配置事实，
    运行期只读，同时可被 provider 层投影安全共享（无需复制）。
    """

    model_config = ConfigDict(frozen=True, extra="ignore")
    """extra="ignore"：未来新增能力（audio 等）时旧版本忽略未知键而非拒绝配置。"""

    vision: bool = S(
        doc="是否接受图片输入（本期唯一能力）",
        notes="未声明或 false 时，ReadImage 不会为该模型附图片（请求里只留占位文本）。",
        apply=ApplyScope.HOT,
        default=False,
    )


class ModelSpec(BaseModel):
    """models 列表项的对象形态（新声明协议）。

    字符串形态（存量）等价于只有 name 的 ModelSpec——两种形态经
    ProviderConfig.model_specs() / model_names() 统一出口消费。
    """

    model_config = ConfigDict(extra="ignore")
    """extra="ignore"：未来新增字段由新版本消费，旧版本忽略未知键。"""

    id: str | None = S(
        doc="全局唯一引用词（缺省 = name）",
        notes=(
            "声明后，一切请求 / 协议 / metadata 引用它；name 只作为发给上游的调用名。\n"
            "允许 : / @ 等可见字符（外部系统的值域不能比我们窄），但不接受首尾空白 / "
            "控制字符 / 空串 / 超长——引用词是查表的键，静默改写会在两台机器上得到"
            "不同的查找结果。"
        ),
        apply=ApplyScope.HOT,
        default=None,
    )
    name: str = S(
        doc="实际调用名（传给 provider.generate）",
        apply=ApplyScope.HOT,
    )
    display_name: str | None = S(
        doc="展示名（缺省由前端回落调用名）",
        apply=ApplyScope.HOT,
        default=None,
    )
    description: str | None = S(
        doc="模型描述（人类可读，可选）",
        apply=ApplyScope.HOT,
        default=None,
    )
    capabilities: ModelCapabilities = S(
        doc="能力声明（未声明 = 全 false 的安全默认）",
        apply=ApplyScope.HOT,
        default_factory=ModelCapabilities,
    )

    @field_validator("name")
    @classmethod
    def _name_valid(cls, v: str) -> str:
        """调用名非空且无首尾空白。

        「无首尾空白」是**隐式 id 的前提**：``effective_id = id or name``，而
        ``find_model()`` 按 trim 后的键查表——name 若带空白，id 空间就会出现
        「查不到自己声明」的破洞（加载说合法、解析说未知），或把请求静默落到
        另一个模型上。文案的唯一实现在 ``problems.model_name_message``
        （跨字段检查与裸字符串形态共用它，不许抄两份）。
        """
        message = model_name_message(v)
        if message is not None:
            raise ValueError(message)
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
        """生效的引用词：显式 id，缺省回落调用名（存量形态零改动）。

        两个来源都保证无首尾空白（``name`` / ``id`` 校验），所以 id 空间的键就是
        ``Config.find_model()`` trim 查找时用的键。
        """
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

    name: str = S(
        doc="Provider 标识（全局唯一）",
        notes="只接受 [a-zA-Z0-9_-]+；它是运行期事实维度，不是模型引用词。",
        apply=ApplyScope.NEXT_SESSION,
        # pattern 同时进 Field 元数据（catalog 的面板侧格式校验读它）；
        # 文案仍由下面的校验器给（pydantic 的 string_pattern_mismatch 会改文案）。
        pattern=r"^[a-zA-Z0-9_-]+$",
    )
    protocol: Literal["openai", "anthropic"] = S(
        doc="协议类型",
        apply=ApplyScope.HOT,
        default="openai",
        choices={
            "openai": "OpenAI 兼容协议（/chat/completions）",
            "anthropic": "Anthropic Messages 协议",
        },
    )
    base_url: str = S(
        doc="服务端点",
        example="https://api.openai.com/v1",
        apply=ApplyScope.HOT,
    )
    api_key: str = S(
        doc="API 密钥",
        apply=ApplyScope.HOT,
        secret=True,
    )
    timeout_first_chunk: float = S(
        doc="流式首块超时（秒）",
        notes="这是**响应头**超时；响应体停滞另有硬编码 120s 判定（provider/transport.py）。",
        apply=ApplyScope.HOT,
        default=300.0,
    )
    timeout_total: float = S(
        doc="非流式调用总超时（秒）",
        notes="流式调用只受首块超时约束。",
        apply=ApplyScope.HOT,
        default=600.0,
    )
    max_retries: int = S(
        doc="LLM 调用失败时的最大重试次数",
        apply=ApplyScope.HOT,
        default=10,
    )
    max_retry_delay: float = S(
        doc="指数退避最大重试间隔（秒）",
        notes="退避被钳制在该上界（默认 3 分钟）。",
        apply=ApplyScope.HOT,
        default=180.0,
    )
    explicit_cache_mode: bool = S(
        doc="显式缓存模式（追加 cache_control 标记）",
        notes="给最后一个 content block 追加 cache_control ephemeral 标记；不支持的 provider 静默忽略。",
        apply=ApplyScope.HOT,
        default=True,
    )
    reasoning_effort: str | None = S(
        doc="推理强度",
        notes="low / medium / high / max；null = 交给 provider 决定。",
        apply=ApplyScope.HOT,
        default=None,
        choices={
            "low": "低推理强度（更快、更省）",
            "medium": "中等推理强度",
            "high": "高推理强度",
            "max": "最高推理强度",
        },
    )
    max_tokens: int = S(
        doc="最大输出 token 数（Anthropic 协议必填）",
        apply=ApplyScope.HOT,
        default=128_000,
    )
    extra_body: dict = S(
        doc="透传到 request body 顶层的额外字段",
        notes=(
            '面板里用单行 JSON 编辑器。例：{"thinking":{"type":"enabled"}}。\n'
            "openai 协议下 enable_thinking / preserve_thinking 默认随每个请求发送；"
            "在这里显式写出可覆盖。"
        ),
        apply=ApplyScope.HOT,
        default_factory=dict,
    )
    anthropic_version: str = S(
        doc="Anthropic API 版本 header",
        notes="仅 anthropic 协议使用。",
        apply=ApplyScope.HOT,
        default="2023-06-01",
    )
    models: list[str | ModelSpec] = S(
        doc="模型声明（目录的唯一来源）",
        notes=(
            "三种形态：裸字符串（id=name）/ 对象（id=name）/ 对象带显式 id。\n"
            "没有远端 /models 发现；至少声明一个。"
        ),
        apply=ApplyScope.HOT,
        default_factory=list,
        min_items=1,
        summary_fields=["id", "name", "display_name"],
    )
    image_delivery: Literal["inline", "followup"] | None = S(
        doc="图片投递形态",
        notes="null = 按协议默认（openai → followup；anthropic → inline）。",
        apply=ApplyScope.HOT,
        default=None,
        choices={
            "inline": "图片留在原消息里（tool_result 内嵌 image block）",
            "followup": "图片汇总为段后的 user 消息（最宽兼容）",
        },
    )
    image_max_bytes: int | None = S(
        doc="单图请求期兜底上限（字节）",
        notes="超过的图片位降级为占位文本；null = 不设上限。",
        apply=ApplyScope.HOT,
        default=None,
    )

    @field_validator("name", mode="before")
    @classmethod
    def _name_valid(cls, v: object) -> object:
        """provider name 只接受 ``[a-zA-Z0-9_-]+``。

        ``mode="before"``：抢在同名 ``Field(pattern=...)`` 之前给出**自有文案**
        （pydantic 的 ``string_pattern_mismatch`` 会改变错误文案，而加载期文案是
        对外契约）。非字符串输入原样放行，交给 pydantic 的类型检查报错。
        """
        import re

        if not isinstance(v, str):
            return v
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
        解析期报错。dup 检查体在 ``problems.provider_model_problems``（同一份实现，
        Config 级检查共用），这里只 raise 第一个 problem 的文案。

        ``model_names()`` 那一行是**裸字符串形态**的调用名守门（非空 / 无首尾空白）：
        对象形态由 ``ModelSpec`` 的字段校验器负责，字符串形态今天经「包装成
        ``ModelSpec``」触发同一校验器——保留原路径，错误文案与 loc 因此逐字不变。
        """
        self.model_names()
        problems = provider_model_problems(self)
        if problems:
            raise ValueError(problems[0].render())
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
    preset: Literal["opencode", "qwen-code"] = S(
        doc="HTTP User-Agent 预设",
        apply=ApplyScope.HOT,
        default="qwen-code",
        choices={
            "opencode": "UA 形如 opencode/<版本>（system release; arch）",
            "qwen-code": "UA 形如 QwenCode/<版本>，并附带 X-DashScope-* 请求头",
        },
    )


class LogConfig(BaseModel):
    level: str = S(
        doc="网关控制台日志级别",
        notes=(
            "daemon 的 stdout/stderr（日志经 wing 侧重定向到 "
            "~/.wing/core/logs/gateway.log）。\n"
            "每日文件日志（wing_YYYY-MM-DD.log）恒为 DEBUG，不受此项影响。"
        ),
        apply=ApplyScope.HOT,
        default="WARNING",
        choices={
            "DEBUG": "最详细（含第三方库）",
            "INFO": "常规运行信息",
            "WARNING": "警告与错误",
            "ERROR": "仅错误",
            "CRITICAL": "仅致命错误",
        },
    )


class EvictionConfig(BaseModel):
    """空闲会话逐出（eviction）配置。

    逐出只回收内存态（worker / provider client），磁盘状态一概不动：
    被逐出的会话在下一次被需要时按需水合（resume / subscribe / send）。
    钉住条件（不逐出）见 SessionManager._blocked_reason。
    """

    model_config = ConfigDict(extra="ignore")

    enabled: bool = S(
        doc="总开关（关闭后 reaper 不扫描）",
        notes="调试 / 保守回滚用。",
        apply=ApplyScope.HOT,
        default=True,
    )
    idle_ttl_seconds: float = S(
        doc="空闲时长阈值（秒）",
        notes="无订阅且超过该时长即逐出；计时器由会话状态变化重置。",
        apply=ApplyScope.HOT,
        default=1800.0,
        gt=0,
    )
    sweep_interval_seconds: float = S(
        doc="扫描周期（秒）",
        notes="启动时读取；热重载不改变已注册 job 的间隔。",
        apply=ApplyScope.RESTART,
        default=300.0,
        gt=0,
    )


class SessionsConfig(BaseModel):
    model_config = ConfigDict(extra="ignore")

    eviction: EvictionConfig = S(
        doc="空闲会话逐出（只回收内存态，磁盘不动）",
        notes=(
            "只有同时满足「没有轮次在跑、没有排队输入、没有客户端订阅、空闲超过阈值」"
            "才会被逐出；空闲计时器随会话状态变化重置。\n"
            "被逐出的会话在下次被需要时按需水合（resume / subscribe / send）。\n"
            "存储路径不是配置字段：由 WING_SESSIONS_PATH / WING_HOME 决定。\n"
            "想立刻回收某个会话用 `wing release <session-id>`。"
        ),
        apply=ApplyScope.RESTART,
        default_factory=EvictionConfig,
    )

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

    name: str = S(
        doc="Agent 名称（全局唯一）",
        apply=ApplyScope.NEXT_SESSION,
    )
    model: str = S(
        doc="引用的 model id（∈ providers[].models 声明的 id 空间）",
        notes="配置加载期强制：未命中即报错。",
        apply=ApplyScope.NEXT_SESSION,
    )
    default: bool = S(
        doc="是否默认 agent（未显式选择时使用）",
        apply=ApplyScope.NEXT_SESSION,
        default=False,
    )
    system_prompt: str = S(
        doc="系统提示词（附加在每次对话开头）",
        apply=ApplyScope.NEXT_SESSION,
        default="",
    )
    tools: list[str] = S(
        doc="可用工具名列表（注册表中的名字）",
        apply=ApplyScope.NEXT_SESSION,
        default_factory=list,
    )
    context_window_tokens: int = S(
        doc="上下文窗口 token 上限",
        notes="达到后触发压缩。",
        apply=ApplyScope.NEXT_SESSION,
        default=256_000,
    )
    keep_recent_tokens: int = S(
        doc="压缩后保留的近期 token 数",
        apply=ApplyScope.NEXT_SESSION,
        default=50_000,
    )
    skills: list[str] = S(
        doc="技能 glob 模式",
        notes="每个匹配加载一个 SKILL.md 作为 agent 上下文。",
        apply=ApplyScope.NEXT_SESSION,
        default_factory=list,
    )
    rules: list[str] = S(
        doc="规则 glob 模式",
        notes="每个匹配加载一个 markdown 文件作为 agent 规则。",
        apply=ApplyScope.NEXT_SESSION,
        default_factory=list,
    )
    max_turns: int | None = S(
        doc="单轮最大工具调用轮数",
        notes="null = 不限制。",
        apply=ApplyScope.NEXT_SESSION,
        default=None,
    )
    yolo: bool | None = S(
        doc="该 agent 是否跳过危险命令确认",
        notes="null = 跟随顶层 yolo。",
        apply=ApplyScope.NEXT_SESSION,
        default=None,
    )


class ToolResultTruncateConfig(BaseModel):
    """Tool result truncation policy.

    max_length: trigger threshold in chars. None or <0 disables truncation.
    keep_chars: number of chars to keep at head and tail when truncating.
    """

    max_length: int | None = S(
        doc="触发截断的字符阈值",
        notes=(
            "null 或负数 = 关闭截断。\n超限的完整结果存到临时文件，上下文里只留头尾。"
        ),
        apply=ApplyScope.HOT,
        default=100_000,
    )
    keep_chars: int = S(
        doc="截断时头 / 尾各保留的字符数",
        apply=ApplyScope.HOT,
        default=200,
    )

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

    max_bytes: int = S(
        doc="单图原始字节上限（默认 4.5 MiB）",
        notes=(
            "ReadImage 读时拒绝 + 降采样提示。\n"
            "4.5 MiB 是对多数 provider 服务端单图上限（约 5 MB 档）的保守取值；"
            "你的 provider 接受更大时可以调大。"
        ),
        apply=ApplyScope.HOT,
        default=4_718_592,
        gt=0,
    )
    max_images: int = S(
        doc="计数高水位（超出即批量驱逐）",
        notes="从最旧开始按 count_quantum 批量丢（KV-cache 友好）。",
        apply=ApplyScope.HOT,
        default=32,
        gt=0,
    )
    count_quantum: int = S(
        doc="计数驱逐量子（每次超限批量丢这么多张）",
        apply=ApplyScope.HOT,
        default=8,
        gt=0,
    )
    request_budget_bytes: int = S(
        doc="请求内图片累计字节高水位（base64 口径）",
        notes="36 MiB ≈ DeepSeek 48 MiB 的 75%。",
        apply=ApplyScope.HOT,
        default=37_748_736,
        gt=0,
    )
    evict_quantum_bytes: int = S(
        doc="字节驱逐量子（= 预算一半，与 DSH 同构）",
        apply=ApplyScope.HOT,
        default=18_874_368,
        gt=0,
    )


class ApiKeyEntry(BaseModel):
    """Single API key with an identity role.

    Keys are restricted to ASCII printable characters (0x20–0x7E).
    HTTP headers are latin-1 encoded; non-ASCII keys would silently
    mismatch between client and server.
    """

    key: str = S(
        doc="API 密钥",
        notes=(
            "仅限 ASCII 可打印字符（0x20–0x7E）：HTTP header 是 latin-1 编码，"
            "非 ASCII 会在两端静默错配。"
        ),
        apply=ApplyScope.HOT,
        secret=True,
    )
    role: str = S(
        doc="密钥角色（RBAC）",
        apply=ApplyScope.HOT,
        default="admin",
        choices={
            "admin": "全量访问（隐式的非受限角色）",
            "tool_runtime": "仅允许注册远程工具（并持有其 WS）",
        },
    )

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

    enabled: bool = S(
        doc="是否启用 API key 鉴权",
        notes=(
            "启用后除豁免路径（/api/health）外全部 HTTP/WS 请求必须携带有效 key："
            "Authorization: Bearer / X-API-Key / WS ?api_key=。\n"
            "WS 的 query 参数写法会被反向代理的访问日志记下，能改 header 时优先用 header。"
        ),
        apply=ApplyScope.HOT,
        default=False,
    )
    keys: list[ApiKeyEntry] = S(
        doc="API key 列表",
        notes="enabled=true 且 keys 为空会锁死全部请求（含 reload），启动时告警。",
        apply=ApplyScope.HOT,
        default_factory=list,
        # 刻意**不**声明 identity_field：元素里唯一的非密文标量是 role（默认 admin，
        # 多条 key 同 role 是常态——不是身份），key 本身是密文（incoming 侧恰好是 null，
        # 根本不能当配对键）。无身份的密文列表走「长度相等时安全下标回落 / 否则宁可不猜」
        # （document.resolve_secrets），删条目时密钥被丢弃 + 回执要求重填。
    )

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
    host: str = S(
        doc="监听地址",
        apply=ApplyScope.RESTART,
        default="127.0.0.1",
    )
    port: int = S(
        doc="监听端口",
        apply=ApplyScope.RESTART,
        default=32523,
    )
    auth: AuthConfig = S(
        doc="API key 鉴权配置",
        apply=ApplyScope.HOT,
        default_factory=AuthConfig,
    )
    remote_tool_timeout: float = S(
        doc="远程工具调用总超时（秒）",
        notes=(
            "安全网，非小超时：远程工具（如 Bash）可能执行很久，默认宽口径 30 分钟。\n"
            "断连是首要失败信号（WS 关闭立即 fail 在途调用），此值仅兜底客户端静默挂死。"
        ),
        apply=ApplyScope.HOT,
        default=1800.0,
    )


class CommandsConfig(BaseModel):
    paths: list[str] = S(
        doc="prompt 命令定义文件的 glob 模式",
        notes=(
            "每个 .md（YAML frontmatter: name/description/aliases；正文用 $ARGUMENTS）"
            "定义一个斜杠命令。例：~/.wing/commands/*.md"
        ),
        apply=ApplyScope.HOT,
        default_factory=list,
    )


class Config(BaseModel):
    model_config = ConfigDict(extra="ignore")

    # ── Providers ──────────────────────────────────
    providers: list[ProviderConfig] = S(
        doc="LLM provider 声明（模型目录的唯一来源）",
        notes=(
            "至少一个；providers[].models 声明的 id 是全局唯一的模型引用词。\n"
            "每个 provider 声明协议（openai / anthropic）、端点与凭据；models 至少一个\n"
            "（没有远端 /models 发现），三条声明形态：\n"
            "  - dfmodel                        # 裸字符串：id = 调用名\n"
            "  - name: dfmodel-2026             # 对象：id = name\n"
            "    display_name: DeepSeek-Flash\n"
            "  - id: ds-flash                   # 显式 id（全局唯一引用词）\n"
            "    name: dfmodel-2026             # 实际发给上游的调用名\n"
            "    capabilities: {vision: true}   # 未声明 = 纯文本（ReadImage 不附图）\n"
            "Anthropic 协议最小示例（max_tokens 必填；image_delivery 默认 inline）：\n"
            "  - name: claude\n"
            "    protocol: anthropic\n"
            "    base_url: https://api.anthropic.com\n"
            "    api_key: sk-ant-xxx\n"
            '    anthropic_version: "2023-06-01"\n'
            "    max_tokens: 8192\n"
            "    models: [claude-sonnet-4-20250514]\n"
            "    extra_body: {thinking: {type: enabled, budget_tokens: 4096}}"
        ),
        apply=ApplyScope.NEXT_SESSION,
        min_items=1,
        summary_fields=["name", "protocol", "base_url"],
        # 身份字段（保存时密文回填的配对键）：name 由跨字段检查强制全局唯一
        # （problems.cross_field_problems 的 duplicate provider name），且非密文——
        # 删 / 移 / 前插 provider 后 api_key 的 null 哨兵按它回填，不会错配到别的 provider。
        identity_field="name",
        section="Providers",
        section_doc="LLM provider 与模型目录（目录只有一个来源：这里的声明）",
    )
    # ── Agents ─────────────────────────────────────
    agents: list[AgentConfig] = S(
        doc="Agent 模板（至少一个）",
        notes="每个 agent 定义模型、工具集与系统提示词。",
        apply=ApplyScope.NEXT_SESSION,
        min_items=1,
        summary_fields=["name", "model"],
        section="Agents",
        section_doc="Agent 模板：模型引用 / 工具集 / 提示词 / skills 与 rules",
    )
    # ── Behavior ───────────────────────────────────
    safe_command_patterns: list[str] = S(
        doc="自动放行的命令正则（不需要确认）",
        notes=r"例：^git\s+(status|log|diff)",
        apply=ApplyScope.HOT,
        default_factory=list,
        section="Behavior",
        section_doc="Agent 行为与内置工具的通用开关（bash 安全 / 结果截断）",
    )
    yolo: bool = S(
        doc="跳过危险命令的安全检查",
        notes="谨慎使用——所有命令不经确认直接执行。",
        apply=ApplyScope.NEXT_SESSION,
        default=False,
        section="Behavior",
    )
    steer: bool = S(
        doc="启用 steer 模式（用引导提示约束 agent 行为）",
        apply=ApplyScope.NEXT_SESSION,
        default=True,
        section="Behavior",
    )
    tool_result_truncate: ToolResultTruncateConfig = S(
        doc="工具结果截断策略",
        apply=ApplyScope.HOT,
        default_factory=ToolResultTruncateConfig,
        section="Behavior",
    )
    # ── Images ─────────────────────────────────────
    images: ImagesConfig = S(
        doc="图片读入与请求期保留预算",
        apply=ApplyScope.HOT,
        default_factory=ImagesConfig,
        section="Images",
        section_doc="ReadImage 与请求期图片投影（预算按请求生效）",
    )
    # ── Sessions ───────────────────────────────────
    sessions: SessionsConfig = S(
        doc="会话管理",
        apply=ApplyScope.RESTART,
        default_factory=SessionsConfig,
        section="Sessions",
        section_doc="会话内存态回收（磁盘状态一概不动）",
    )
    # ── Gateway ────────────────────────────────────
    gateway: GatewayConfig = S(
        doc="网关服务配置",
        apply=ApplyScope.RESTART,
        default_factory=GatewayConfig,
        section="Gateway",
        section_doc="网关监听 / 鉴权 / 远程工具",
    )
    # ── Extensibility ──────────────────────────────
    hooks: list[str] = S(
        doc="hook 文件的 glob 模式",
        notes="hook 是 Python 模块，经 wing hook API 注册处理器。",
        apply=ApplyScope.HOT,
        default_factory=list,
        section="Extensibility",
        section_doc="扩展点：hooks 与 prompt 命令",
    )
    commands: CommandsConfig = S(
        doc="prompt 命令配置",
        apply=ApplyScope.HOT,
        default_factory=CommandsConfig,
        section="Extensibility",
    )
    # ── Logging ────────────────────────────────────
    log: LogConfig = S(
        doc="日志配置",
        apply=ApplyScope.HOT,
        default_factory=LogConfig,
        section="Logging",
        section_doc="日志（控制台级别；文件日志恒为 DEBUG）",
    )
    # ── Advanced ───────────────────────────────────
    user_agent: UserAgentConfig = S(
        doc="HTTP User-Agent 预设",
        apply=ApplyScope.HOT,
        default_factory=UserAgentConfig,
        section="Advanced",
        section_doc="低层 / 少用开关",
    )

    @model_validator(mode="after")
    def _validate_config(self) -> "Config":
        """跨字段检查（检查体住在 ``wing.config.problems``）。

        加载期只报第一个（pydantic 契约），文案与抽取前逐字一致；设置面板路径
        用同一批纯函数拿**全部**问题（含精确路径）。
        """
        problems = cross_field_problems(self)
        if problems:
            raise ValueError(problems[0].render())
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

        文案的唯一实现在 ``problems.unknown_model_message``（跨字段检查的
        ``agents[].model`` 那条共用它，所以这里委托而不是抄一份）。
        """
        return unknown_model_message(list(iter_model_refs(self.providers)), model_id)

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
