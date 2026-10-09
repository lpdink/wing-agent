"""ProbeEnv —— 确定性集成环境的自举与停止（spec「环境自举与隔离」）。

一次 ``ProbeEnv.start(root)`` 得到：

- 临时 ``WING_HOME``（``root/wing_home``，含生成的 ``core/config.yaml``，
  providers 指向假 Provider）与 ``WING_SESSIONS_PATH``（``root/sessions``）——
  绝不读写用户真实 ``~/.wing``；
- 进程内假 Provider（aiohttp，OS 分配端口，见 ``wing_probe.provider``）；
- 网关子进程 ``wing-gateway -p <port>``（stdout/stderr 落盘 ``root/gateway.log``），
  启动后轮询 ``/api/health`` 直至就绪。

停止走 ``POST /api/shutdown`` → 等待退出（≤5s）→ ``terminate`` → ``kill``，
幂等且不抛（场景失败路径也要能安全收尾）。

端口策略：假 Provider 用 ``port=0`` 由 OS 分配后读回真实端口；网关端口
``bind(0)`` 预留（关闭 socket 后存在竞态）——启动即退出（典型端口冲突）时
换端口重试，最多 ``gateway_attempts`` 次。
"""

from __future__ import annotations

import asyncio
import logging
import os
import shutil
import socket
import subprocess
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import IO, Any

import httpx
import yaml

from wing_probe.provider.script import Script
from wing_probe.provider.server import FakeProvider

_log = logging.getLogger("wing_probe.env")

DEFAULT_HOST = "127.0.0.1"
DEFAULT_HEALTH_TIMEOUT = 30.0
DEFAULT_GATEWAY_ATTEMPTS = 3
DEFAULT_SHUTDOWN_WAIT = 5.0
HEALTH_POLL_INTERVAL = 0.05
HEALTH_REQUEST_TIMEOUT = 2.0
LOG_TAIL_LINES = 40
LOG_TAIL_CHARS = 4000

#: loopback 主机名：probe 的 HTTP 客户端只连本机网关 / 假 Provider，必须绕过
#: 环境与系统代理——`urllib.request.getproxies()` 会把 macOS 系统代理（scutil）
#: 喂给 httpx，连 `127.0.0.1` 也被送进代理（表现为 health 502，整组场景假红）。
#: 这不是"要不要走外网"的取舍：probe 的语义就是本机闭环。
LOOPBACK_HOSTS: tuple[str, ...] = ("127.0.0.1", "localhost")

#: agents[].model 占位（场景经 AgentOverride 覆盖为真实 probe 模型）。
DEFAULT_PROBE_MODEL = "probe/default"
DEFAULT_PROVIDER_NAME = "probe"
DEFAULT_PROVIDER_API_KEY = "probe-key"

#: 默认模板的 system prompt（**非空**：system 段要真的参与请求组装 / compact /
#: KV cache 前缀语义，"system + 摘要"这类断言才不是空串上的退化；内容固定，
#: 场景可直接与 ``probe.system_prompt`` 对照）。
DEFAULT_SYSTEM_PROMPT = (
    "You are wing's deterministic probe agent. Answer the user's request directly "
    "and keep the answer short."
)

#: 预置 default 模板的工具集（内置工具，无远程宿主依赖）。
DEFAULT_AGENT_TOOLS: tuple[str, ...] = (
    "Bash",
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "AskUserQuestion",
    "TodoWrite",
)

#: 网关重试次数：语义重试场景（无效轮次）需要它 >0；保持有界、可数
#: （一次逻辑调用的剧本消费上界 = 1 + max_retries）。退避压到 1s
#: （``max_retry_delay``）保证场景快。
PROBE_MAX_RETRIES = 2

#: 默认模板的上下文窗口 / 保留区（与产品默认同量级）。自动压缩场景用
#: ``@pytest.mark.probe_env(context_window_tokens=…, keep_recent_tokens=…)``
#: 压小它们，让 early trigger / apply 在几轮对话内确定性触发。
DEFAULT_CONTEXT_WINDOW_TOKENS = 256_000
DEFAULT_KEEP_RECENT_TOKENS = 50_000


def conflicting_config_knobs(**values: Any) -> list[str]:
    """``config_text`` 与「生成配置」旋钮互斥的判定（返回显式偏离默认值的旋钮名）。

    ``config_text`` 会**整份替换**生成的 ``config.yaml``——同一份启动参数里再给
    ``models=`` / ``auth=`` 之类的旋钮，它们会被静默忽略（场景作者会对着"自己明明
    声明了模型"的配置调试半天）。所以显式偏离默认值即报错，而不是静默丢弃。
    「显式传了与默认相同的值」不算冲突（无假红）：判定基准就是模块默认常量。
    """
    defaults: dict[str, Any] = {
        "model": DEFAULT_PROBE_MODEL,
        "models": None,
        "extra_providers": None,
        "images": None,
        "provider_extra": None,
        "tools": DEFAULT_AGENT_TOOLS,
        "system_prompt": DEFAULT_SYSTEM_PROMPT,
        "context_window_tokens": DEFAULT_CONTEXT_WINDOW_TOKENS,
        "keep_recent_tokens": DEFAULT_KEEP_RECENT_TOKENS,
        "sessions": None,
        "hooks": (),
        "auth": None,
    }
    conflicts: list[str] = []
    for name, value in values.items():
        default = defaults[name]
        # 序列形态按元素比较（list/tuple 拼写差异不算冲突）。
        if isinstance(default, tuple) and isinstance(value, Sequence):
            differs = tuple(value) != default
        else:
            differs = value != default
        if differs:
            conflicts.append(name)
    return conflicts


class ProbeEnvError(RuntimeError):
    """环境自举失败（二进制缺失 / 进程提前退出 / 健康检查超时）。"""


# ── 端口 ────────────────────────────────────────────────────────


def reserve_port(host: str = DEFAULT_HOST) -> int:
    """``bind(0)`` 预留一个当前空闲端口并立即释放（返回端口号）。

    释放到网关真正监听之间有竞态窗口，所以端口冲突不是错误路径：
    ``ProbeEnv.start_gateway`` 见进程提前退出即换端口重试。
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((host, 0))
        return int(sock.getsockname()[1])


def merge_no_proxy(*values: str | None) -> str:
    """把 loopback 主机并入 ``NO_PROXY`` 值（多来源取并集，保留既有条目，重复不追加）。

    调用方把 ``NO_PROXY`` / ``no_proxy`` 两个拼写都传进来——环境里两种拼写各占
    一半，只读其中一种会让另一种的既有条目丢失。网关子进程继承合并结果：子进程
    里的 httpx（provider 调用假 Provider）同样要绕开环境 / 系统代理（见
    :data:`LOOPBACK_HOSTS`）。``*`` 视为全豁免，原样保留。
    """
    entries: list[str] = []
    covered: set[str] = set()
    for value in values:
        for item in (value or "").split(","):
            entry = item.strip()
            if entry and entry not in covered:
                covered.add(entry)
                entries.append(entry)
    if "*" not in covered:
        for host in LOOPBACK_HOSTS:
            if host not in covered:
                entries.append(host)
    return ",".join(entries)


# ── 网关二进制解析 ──────────────────────────────────────────────


def repo_root() -> Path | None:
    """开发 checkout 下的仓库根；安装到 site-packages 时返回 None。"""
    for parent in Path(__file__).resolve().parents:
        if (parent / "libs" / "core" / "pyproject.toml").is_file():
            return parent
    return None


def candidate_gateway_binaries(
    *,
    executable: str | None = None,
    root: Path | None = None,
) -> list[Path]:
    """候选路径（按解析优先级）：``sys.executable`` 同目录 → 仓库 ``.venv/bin``。"""
    python = Path(executable if executable is not None else sys.executable)
    candidates = [python.parent / "wing-gateway"]
    checkout = root if root is not None else repo_root()
    if checkout is not None:
        candidates.append(checkout / ".venv" / "bin" / "wing-gateway")
    return candidates


def resolve_gateway_bin(
    *,
    env: Mapping[str, str] | None = None,
    executable: str | None = None,
    root: Path | None = None,
    which: Callable[[str], str | None] = shutil.which,
) -> Path:
    """解析 ``wing-gateway`` 可执行文件。

    顺序：``$WING_GATEWAY_BIN`` > ``sys.executable`` 同目录 > 仓库
    ``.venv/bin/wing-gateway`` > ``shutil.which("wing-gateway")``。
    全部失败抛 ``ProbeEnvError``（附解析过程与 ``uv sync`` 提示）。
    """
    environ: Mapping[str, str] = os.environ if env is None else env
    override = environ.get("WING_GATEWAY_BIN")
    tried: list[str] = []
    if override:
        path = Path(override).expanduser()
        if path.is_file():
            return path.resolve()
        raise ProbeEnvError(
            f"$WING_GATEWAY_BIN points to a missing file: {override}\n"
            "fix the variable or unset it to fall back to the search order"
        )
    for candidate in candidate_gateway_binaries(executable=executable, root=root):
        tried.append(str(candidate))
        if candidate.is_file():
            return candidate.resolve()
    found = which("wing-gateway")
    tried.append("shutil.which('wing-gateway')")
    if found:
        return Path(found).resolve()
    raise ProbeEnvError(
        "wing-gateway executable not found; searched:\n  "
        + "\n  ".join(tried)
        + "\nrun `uv sync` in the repository root (or set $WING_GATEWAY_BIN)"
    )


# ── config.yaml 生成 ────────────────────────────────────────────


def model_declaration_id(entry: str | Mapping[str, Any]) -> str:
    """模型声明的 **effective id**（``spec.id or spec.name``，与后端同一条规则）。

    str 形态（存量）= 自身；dict 形态 = ``id`` 非空取 ``id``，否则取 ``name``。
    仅用于配置生成期的「模板 model 是否已在 id 空间」判断（见
    :func:`render_config_yaml`），不是解析——运行期解析只发生在网关里。
    """
    if isinstance(entry, str):
        return entry
    name = entry.get("name")
    if not isinstance(name, str) or not name:
        raise ProbeEnvError(f"model declaration needs a non-empty name: {entry!r}")
    declared = entry.get("id")
    if isinstance(declared, str) and declared:
        return declared
    return name


def model_declaration_name(entry: str | Mapping[str, Any]) -> str:
    """模型声明的**调用名**（发给上游的值；str 形态 = 自身）。"""
    return entry if isinstance(entry, str) else str(entry.get("name") or "")


def resolve_model_declarations(
    *,
    model: str,
    models: Sequence[str | Mapping[str, Any]] | None,
) -> list[str | Mapping[str, Any]]:
    """``providers[0].models`` 的最终声明（模板 model 一定落在 id 空间里）。

    组合规则（新世界：``agents[].model`` 必须 ∈ id 空间，否则网关启动即失败）：

    1. ``models is None`` → ``[model]``：模板 model 自己就是唯一声明（缺省 id = name）；
    2. 显式声明 → 原样保留；模板 ``model`` 既不在声明的 effective id 集合、也不在
       调用名集合里时，末尾**追加**字符串形态的 ``model``（id = name = model）。

    第 2 条的「也不在调用名集合」是一个边界：场景若已声明了同名调用名但给了别的 id，
    追加会撞 ``duplicate model name``（后端拒绝，报错比真实问题更误导），此时不追加
    ——网关会以 ``unknown model id '<model>'`` 明说模板 model 不落在 id 空间。
    """
    if models is None:
        return [model]
    declared: list[str | Mapping[str, Any]] = list(models)
    ids = {model_declaration_id(entry) for entry in declared}
    names = {model_declaration_name(entry) for entry in declared}
    if model not in ids and model not in names:
        declared.append(model)
    return declared


def _default_provider_block(
    *, name: str, base_url: str, api_key: str
) -> dict[str, Any]:
    """provider 条目的公共部分（假 Provider 接线 + 有界重试；两种来源共用）。

    ``max_retries`` / ``max_retry_delay`` 有界且可数：语义重试场景要求
    ``max_retries > 0``，退避压到 1s 保证场景快（一次逻辑调用的剧本消费上界
    = ``1 + max_retries``）。
    """
    return {
        "name": name,
        "protocol": "openai",
        "base_url": base_url.rstrip("/"),
        "api_key": api_key,
        "timeout_first_chunk": 30.0,
        "timeout_total": 120.0,
        "max_retries": PROBE_MAX_RETRIES,
        "max_retry_delay": 1.0,
        "explicit_cache_mode": True,
        "extra_body": {},
    }


def render_config_yaml(
    *,
    provider_base_url: str,
    gateway_port: int,
    provider_name: str = DEFAULT_PROVIDER_NAME,
    provider_api_key: str = DEFAULT_PROVIDER_API_KEY,
    model: str = DEFAULT_PROBE_MODEL,
    models: Sequence[str | Mapping[str, Any]] | None = None,
    extra_providers: Sequence[Mapping[str, Any]] = (),
    images: Mapping[str, Any] | None = None,
    provider_extra: Mapping[str, Any] | None = None,
    tools: Sequence[str] = DEFAULT_AGENT_TOOLS,
    system_prompt: str = DEFAULT_SYSTEM_PROMPT,
    context_window_tokens: int = DEFAULT_CONTEXT_WINDOW_TOKENS,
    keep_recent_tokens: int = DEFAULT_KEEP_RECENT_TOKENS,
    log_level: str = "INFO",
    sessions: Mapping[str, Any] | None = None,
    hooks: Sequence[str] = (),
    auth: Mapping[str, Any] | None = None,
) -> str:
    """生成 probe 网关配置（providers 指向假 Provider；agents 预置 default）。

    base_url 是 OpenAI 兼容根（``http://host:port/v1``）——provider 在其后拼
    ``/chat/completions``（远端模型发现已退役，没有 ``/models`` 这条路）。
    ``system_prompt`` 默认非空（见 :data:`DEFAULT_SYSTEM_PROMPT`），空串会让
    system 段从请求里消失。

    ``models`` 是 provider 静态模型声明列表（元素为 str 或 dict）：None = 只声明
    模板 ``model`` 自己（``[model]``），显式给出时模板 model 不在 id 空间则追加
    ——组合规则见 :func:`resolve_model_declarations`。``agents[].provider`` 不再
    生成（后端已删该字段；provider 是运行期事实，不是引用词）。

    ``extra_providers`` 是**附加 provider** 的条目（跨 provider 场景用）：每个
    条目 ``{"name", "base_url", "models"}``（``api_key`` 可选，缺省同主 provider）
    ——接线与主 provider 逐字段同形（同一份 :func:`_default_provider_block`）。
    ``models`` 必填非空：新世界每个 provider 至少声明一个模型，否则配置加载失败。
    ``ProbeEnv`` 把它的 base_url 指到假 Provider 的第二条路径前缀（见
    :meth:`ProbeEnv.extra_provider_specs`）。

    ``images`` 是顶层 ``images:`` 段的原文（None = 不写）。``sessions`` 是透传给
    配置 ``sessions:`` 段的原文（None = 用默认值；逐出场景靠它把 TTL 压到秒级）。

    ``provider_extra`` 是 provider 级透传旋钮：键值合进 ``providers[0]``
    （None = 不合并，缺省输出与既有形态逐字节一致）——用于覆盖协议级行为
    （如 ``image_delivery: inline``）。**谨慎**：覆盖 ``base_url`` 等接线键会
    断开假 Provider，属场景自伤。

    ``hooks`` 是透传给配置 ``hooks:`` 段的 glob 列表（默认空 = 不加载 hook）。
    相对路径按**网关进程 cwd** 解析——``ProbeEnv`` 以 ``env.root`` 为 cwd 启动
    网关，因此惯例是 ``"hooks/*.py"``：场景在跑起来后把 hook 文件写进
    ``<root>/hooks/``，经 ``POST /api/system/reload`` 加载（见
    scenarios/test_session_persistence.py）。

    ``auth`` 是 ``gateway.auth`` 段的原文（None = 保持缺省
    ``{"enabled": False}``，与既有输出逐字节一致）。非 None 时**整体替换**该段
    （场景负责给全 ``enabled`` 与 ``keys``）——配合 driver 的 ``api_key``
    （``Probe.start``）使用，见 scenarios/test_gateway_auth.py。
    """
    provider: dict[str, Any] = _default_provider_block(
        name=provider_name, base_url=provider_base_url, api_key=provider_api_key
    )
    provider["models"] = resolve_model_declarations(model=model, models=models)
    if provider_extra is not None:
        provider.update(provider_extra)
    providers: list[dict[str, Any]] = [provider]
    for spec in extra_providers:
        extra_name = spec.get("name")
        extra_base_url = spec.get("base_url")
        extra_models = spec.get("models")
        if not isinstance(extra_name, str) or not extra_name:
            raise ProbeEnvError(f"extra provider needs a name: {dict(spec)!r}")
        if not isinstance(extra_base_url, str) or not extra_base_url:
            raise ProbeEnvError(
                f"extra provider '{extra_name}' needs a base_url: {dict(spec)!r}"
            )
        if not extra_models:
            raise ProbeEnvError(
                f"extra provider '{extra_name}' declares no models "
                "(providers[].models must be non-empty)"
            )
        block = _default_provider_block(
            name=extra_name,
            base_url=extra_base_url,
            api_key=str(spec.get("api_key") or provider_api_key),
        )
        block["models"] = list(extra_models)
        providers.append(block)
    config: dict[str, Any] = {
        "providers": providers,
        "agents": [
            {
                "name": "default",
                "model": model,
                "default": True,
                "system_prompt": system_prompt,
                "tools": list(tools),
                "context_window_tokens": context_window_tokens,
                "keep_recent_tokens": keep_recent_tokens,
                "skills": [],
                "rules": [],
            }
        ],
        "hooks": list(hooks),
        "safe_command_patterns": [],
        "yolo": False,
        "log": {"level": log_level},
        "gateway": {
            "host": DEFAULT_HOST,
            "port": gateway_port,
            "auth": dict(auth) if auth is not None else {"enabled": False},
        },
        "commands": {"paths": []},
    }
    if images is not None:
        config["images"] = dict(images)
    if sessions is not None:
        config["sessions"] = dict(sessions)
    header = "# generated by wing-probe ProbeEnv on every start — do not edit\n"
    return header + yaml.safe_dump(config, sort_keys=False, allow_unicode=True)


def write_config(wing_home: str | Path, text: str) -> Path:
    """把配置写到 ``<wing_home>/core/config.yaml``（建目录，返回路径）。"""
    path = Path(wing_home) / "core" / "config.yaml"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


# ── 日志与健康等待 ──────────────────────────────────────────────


def read_log_tail(
    path: str | Path | None,
    *,
    max_lines: int = LOG_TAIL_LINES,
    max_chars: int = LOG_TAIL_CHARS,
) -> str:
    """日志尾部（诊断用）：行数与字符数双上限，永不抛。"""
    if path is None:
        return "(no log file)"
    file_path = Path(path)
    try:
        text = file_path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        return f"(cannot read {file_path}: {exc})"
    body = "\n".join(text.splitlines()[-max_lines:])
    if len(body) > max_chars:
        body = "…" + body[-max_chars:]
    return body or "(log is empty)"


def log_section(path: str | Path | None, title: str = "gateway.log tail") -> str:
    """报告用的日志分节（含路径，便于失败后翻查）。"""
    return f"--- {title} ({path}) ---\n{read_log_tail(path)}"


async def wait_for_health(
    url: str,
    *,
    timeout: float = DEFAULT_HEALTH_TIMEOUT,
    interval: float = HEALTH_POLL_INTERVAL,
    process: subprocess.Popen[bytes] | None = None,
    log_path: str | Path | None = None,
    label: str = "gateway",
) -> None:
    """轮询 ``url`` 直至 200；进程提前退出或超时抛 ``ProbeEnvError``。

    失败报告必须能独立定位：解析出的 url、超时值、进程退出码与日志尾部。
    """
    deadline = time.monotonic() + timeout
    timeout_ctx = httpx.Timeout(HEALTH_REQUEST_TIMEOUT)
    # trust_env=False：只连 loopback，绕开环境 / 系统代理（见 LOOPBACK_HOSTS）。
    async with httpx.AsyncClient(timeout=timeout_ctx, trust_env=False) as client:
        while True:
            if process is not None and process.poll() is not None:
                raise ProbeEnvError(
                    f"{label} exited with code {process.returncode} before "
                    f"becoming healthy ({url})\n{log_section(log_path)}"
                )
            try:
                response = await client.get(url)
            except httpx.HTTPError:
                response = None  # 尚未监听 / 连接被拒
            if response is not None and response.status_code == 200:
                return
            if time.monotonic() >= deadline:
                detail = (
                    f"last status: {response.status_code}"
                    if response is not None
                    else "no response"
                )
                raise ProbeEnvError(
                    f"health check timed out after {timeout:.1f}s: {url} ({detail})\n"
                    f"{log_section(log_path)}"
                )
            await asyncio.sleep(interval)


# ── ProbeEnv ────────────────────────────────────────────────────


class ProbeEnv:
    """一个自举好的 probe 环境（临时 WING_HOME + 假 Provider + 网关子进程）。

    用法（fixture 或脚本）：

        env = await ProbeEnv.start(tmp_path)
        env.register("probe/smoke", Script(Turn.of(text="hi")))
        ...
        await env.stop()
    """

    def __init__(
        self,
        root: str | Path,
        *,
        gateway_bin: str | Path | None = None,
        health_timeout: float = DEFAULT_HEALTH_TIMEOUT,
        gateway_attempts: int = DEFAULT_GATEWAY_ATTEMPTS,
        model: str = DEFAULT_PROBE_MODEL,
        models: Sequence[str | Mapping[str, Any]] | None = None,
        extra_providers: Sequence[Mapping[str, Any]] | None = None,
        images: Mapping[str, Any] | None = None,
        provider_extra: Mapping[str, Any] | None = None,
        tools: Sequence[str] = DEFAULT_AGENT_TOOLS,
        system_prompt: str = DEFAULT_SYSTEM_PROMPT,
        context_window_tokens: int = DEFAULT_CONTEXT_WINDOW_TOKENS,
        keep_recent_tokens: int = DEFAULT_KEEP_RECENT_TOKENS,
        env_overrides: Mapping[str, str] | None = None,
        sessions: Mapping[str, Any] | None = None,
        hooks: Sequence[str] = (),
        auth: Mapping[str, Any] | None = None,
        config_text: str | None = None,
    ) -> None:
        if config_text is not None:
            conflicts = conflicting_config_knobs(
                model=model,
                models=models,
                extra_providers=extra_providers,
                images=images,
                provider_extra=provider_extra,
                tools=tools,
                system_prompt=system_prompt,
                context_window_tokens=context_window_tokens,
                keep_recent_tokens=keep_recent_tokens,
                sessions=sessions,
                hooks=hooks,
                auth=auth,
            )
            if conflicts:
                raise ProbeEnvError(
                    f"config_text replaces the generated config.yaml, so these "
                    f"generation knobs would be silently ignored: {', '.join(conflicts)}"
                )
        self.root = Path(root).expanduser().resolve()
        self.wing_home = self.root / "wing_home"
        self.sessions_path = self.root / "sessions"
        self.artifacts_path = self.root / "artifacts"
        self.log_path = self.root / "gateway.log"
        self.config_path = self.wing_home / "core" / "config.yaml"
        self.model = model
        self.models = tuple(models) if models is not None else None
        """provider 静态模型声明（元素 str 或 dict；None = 只声明模板 model）。"""
        self.extra_providers = (
            tuple(dict(entry) for entry in extra_providers)
            if extra_providers is not None
            else None
        )
        """附加 provider 旋钮（跨 provider 场景；``{"name", "models"}``）。

        base_url 由 :meth:`extra_provider_specs` 指到假 Provider 的第二条路径
        前缀（``/<name>/v1``）——同进程、同剧本表，请求归属靠留档的 path 判定。
        """
        self.images = dict(images) if images is not None else None
        """顶层 images: 段原文（None = 不写该段，用配置缺省值）。"""
        self.provider_extra = (
            dict(provider_extra) if provider_extra is not None else None
        )
        """provider 级透传键（合进 providers[0]；None = 不合并）。"""
        self.tools = tuple(tools)
        self.system_prompt = system_prompt
        """默认模板的 system prompt（非空；场景可与请求里的 system 段对照）。"""
        self.context_window_tokens = context_window_tokens
        """默认模板的上下文窗口（压缩 apply 阈值；压小即得确定性自动压缩）。"""
        self.keep_recent_tokens = keep_recent_tokens
        """默认模板的保留区预算（``compact_window = window - keep_recent``）。"""

        self.auth = dict(auth) if auth is not None else None
        """``gateway.auth`` 段原文（None = 缺省关闭；见 ``render_config_yaml``）。"""

        self.config_text = config_text
        """场景自带的 ``core/config.yaml`` **原文**（None = 按参数生成，既有行为）。

        非 None 时 :meth:`_render_config` 逐字返回它（连故意的 YAML 语法错都能表达）
        ——setup mode 场景用它注入"坏到不能启动"的配置。代价：假 Provider 的接线
        不再自动生成，场景要用 ``env.provider.base_url`` 自己拼 ``base_url``；
        与「配置生成」旋钮互斥（偏离默认值即 ``ProbeEnvError``，见
        :func:`conflicting_config_knobs`）。启动仍带 ``-p <OS 分配端口>``（``cli.py``
        的显式参数优先于配置），所以探针总能连上降级启动的网关。
        """

        self.provider = FakeProvider(
            host=DEFAULT_HOST, path_prefixes=self._extra_provider_names()
        )
        """进程内假 Provider（注册剧本 / 读请求留档）。"""

        self._gateway_bin_override = (
            Path(gateway_bin).expanduser() if gateway_bin is not None else None
        )
        self._health_timeout = health_timeout
        self._gateway_attempts = gateway_attempts
        self._env_overrides = dict(env_overrides or {})
        self._sessions_config = dict(sessions) if sessions is not None else None
        self._hooks_config = tuple(hooks)
        self._process: subprocess.Popen[bytes] | None = None
        self._log_handle: IO[bytes] | None = None
        self._port: int | None = None
        self._gateway_bin: Path | None = None
        self._stopped = False
        self.started_at = time.monotonic()
        """相对单调时钟基准（事件时间线用；见 design D4）。"""

    # ── 启动 ────────────────────────────────────────────────

    @classmethod
    async def start(
        cls,
        root: str | Path,
        **kwargs: Any,
    ) -> ProbeEnv:
        """自举整套环境；中途失败时已起的部分会被收尾再抛错。"""
        env = cls(root, **kwargs)
        try:
            await env.start_provider()
            await env.start_gateway()
        except BaseException:
            await env.stop()
            raise
        return env

    async def start_provider(self) -> FakeProvider:
        """起假 Provider（OS 分配端口，读回真实端口）。"""
        if not self.provider.started:
            await self.provider.start()
        return self.provider

    async def start_gateway(self) -> None:
        """写配置 → 起网关子进程 → 等健康（端口冲突换端口重试）。"""
        if self._process is not None and self._process.poll() is None:
            return
        self._prepare_dirs()
        binary = self._resolve_binary()
        last_error: ProbeEnvError | None = None
        for attempt in range(1, self._gateway_attempts + 1):
            port = reserve_port(DEFAULT_HOST)
            write_config(self.wing_home, self._render_config(port))
            self._port = port
            self._spawn(binary, port, attempt=attempt)
            try:
                await wait_for_health(
                    f"{self.gateway_url}/api/health",
                    timeout=self._health_timeout,
                    process=self._process,
                    log_path=self.log_path,
                    label=(
                        f"wing-gateway [{' '.join(self.command())}] "
                        f"(attempt {attempt}/{self._gateway_attempts})"
                    ),
                )
            except ProbeEnvError as exc:
                last_error = exc
                exited = self._process is not None and self._process.poll() is not None
                await self._terminate_process()
                if exited and attempt < self._gateway_attempts:
                    _log.warning("gateway died on start (attempt %d): %s", attempt, exc)
                    continue  # 大概率端口冲突：换端口重试
                raise ProbeEnvError(f"{exc}\n{self.startup_report(binary)}") from exc
            self._log_marker(f"--- probe: gateway healthy on port {port} ---")
            return
        raise last_error if last_error is not None else ProbeEnvError("gateway failed")

    async def restart_gateway(self) -> None:
        """重启网关子进程（同一套隔离目录，新端口，假 Provider 不重启）。

        ``stop()`` 是**单向闸门**（实例报废、不支持同实例再起）；而"进程跑起来
        → 杀掉 → 在同一 ``WING_HOME`` / ``WING_SESSIONS_PATH`` 上重新跑"是另一
        回事：重启的是**被测进程**，隔离目录与假 Provider（剧本 + 请求留档）都
        要连续——这正是"跨进程重启"语义的观测前提。

        做的事：``POST /api/shutdown`` → 等退出（同 ``stop`` 的收尾纪律）→
        ``start_gateway()``（换端口 + 健康检查）。失败即抛，不带半死进程返回。

        Raises:
            ProbeEnvError: env 已 stop（单向闸门之外）或新进程起不来。
        """
        if self._stopped:
            raise ProbeEnvError(
                "env is stopped (stop() is a one-way gate); create a new "
                "ProbeEnv instead of restarting a dead one"
            )
        if self._process is not None and self._process.poll() is None:
            await self._request_shutdown()
        await self._terminate_process()
        self._log_marker("--- probe: gateway restart ---")
        await self.start_gateway()

    def _extra_provider_names(self) -> tuple[str, ...]:
        """附加 provider 的 name 列表（假 Provider 注册路径前缀用；构造期就绪）。"""
        names: list[str] = []
        for entry in self.extra_providers or ():
            name = entry.get("name")
            if not isinstance(name, str) or not name:
                raise ProbeEnvError(f"extra provider needs a name: {entry!r}")
            names.append(name)
        return tuple(names)

    def extra_provider_specs(self) -> list[dict[str, Any]]:
        """附加 provider 的完整条目（``render_config_yaml`` 消费的形态）。

        base_url = ``<假 Provider 地址>/<name>/v1``——假 Provider 按构造参数
        ``path_prefixes`` 为每个 name 注册同一条 chat completions 路由，于是
        「这次调用打到哪个 provider」由请求留档的 ``path`` 判定。
        """
        specs: list[dict[str, Any]] = []
        for entry in self.extra_providers or ():
            name = entry.get("name")
            models = entry.get("models")
            if not models:
                raise ProbeEnvError(
                    f"extra provider {name!r} declares no models "
                    "(providers[].models must be non-empty)"
                )
            specs.append(
                {
                    "name": name,
                    "base_url": f"{self.provider.url}/{name}/v1",
                    "models": list(models),
                }
            )
        return specs

    def _render_config(self, port: int) -> str:
        """按当前参数生成配置文本（启动重试换端口时重新生成）。

        ``config_text`` 非 None ⇒ 逐字返回场景自带的文件（不再生成；两者的关系
        见 :attr:`config_text` 与 :func:`conflicting_config_knobs`）。
        """
        if self.config_text is not None:
            return self.config_text
        return render_config_yaml(
            provider_base_url=self.provider.base_url,
            gateway_port=port,
            model=self.model,
            models=self.models,
            extra_providers=self.extra_provider_specs(),
            images=self.images,
            provider_extra=self.provider_extra,
            tools=self.tools,
            system_prompt=self.system_prompt,
            context_window_tokens=self.context_window_tokens,
            keep_recent_tokens=self.keep_recent_tokens,
            sessions=self._sessions_config,
            hooks=self._hooks_config,
            auth=self.auth,
        )

    def startup_report(self, binary: Path | None = None) -> str:
        """启动上下文（解析结果 / 命令 / 隔离目录 / 假 Provider），失败报告用。"""
        resolved = binary or self._gateway_bin
        lines = [
            "--- probe startup context ---",
            f"  binary: {resolved}",
            f"  command: {' '.join(self.command())}",
            f"  WING_HOME: {self.wing_home}",
            f"  WING_SESSIONS_PATH: {self.sessions_path}",
            f"  provider: {self.provider_url}",
            f"  health timeout: {self._health_timeout:.1f}s",
            f"  log: {self.log_path}",
        ]
        for spec in self.extra_provider_specs():
            lines.append(f"  extra provider: {spec['name']} → {spec['base_url']}")
        return "\n".join(lines)

    def _prepare_dirs(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        self.wing_home.mkdir(parents=True, exist_ok=True)
        self.sessions_path.mkdir(parents=True, exist_ok=True)
        self.artifacts_path.mkdir(parents=True, exist_ok=True)

    def _resolve_binary(self) -> Path:
        if self._gateway_bin is None:
            if self._gateway_bin_override is not None:
                if not self._gateway_bin_override.is_file():
                    raise ProbeEnvError(
                        "gateway_bin override does not exist: "
                        f"{self._gateway_bin_override}"
                    )
                self._gateway_bin = self._gateway_bin_override.resolve()
            else:
                self._gateway_bin = resolve_gateway_bin()
        return self._gateway_bin

    def env_vars(self) -> dict[str, str]:
        """网关子进程环境：显式指向 tmp（防外部环境污染，design D7）。

        同时把 loopback 并入 ``NO_PROXY``：子进程的 httpx 打假 Provider 时同样
        绕开环境 / 系统代理（见 :data:`LOOPBACK_HOSTS`）。
        """
        env = dict(os.environ)
        env["WING_HOME"] = str(self.wing_home)
        env["WING_SESSIONS_PATH"] = str(self.sessions_path)
        env["PYTHONUNBUFFERED"] = "1"
        env["NO_PROXY"] = merge_no_proxy(env.get("NO_PROXY"), env.get("no_proxy"))
        env["no_proxy"] = env["NO_PROXY"]
        env.update(self._env_overrides)
        return env

    def command(self, port: int | None = None) -> list[str]:
        """本次（或给定端口的）启动命令——报告用。"""
        binary = self._gateway_bin or self._gateway_bin_override
        target_port = self._port if port is None else port
        return [str(binary or "<wing-gateway>"), "-p", str(target_port or 0)]

    def _spawn(self, binary: Path, port: int, *, attempt: int) -> None:
        self._close_log()
        self._log_handle = open(self.log_path, "ab")
        self._log_marker(
            f"--- probe: starting {binary} -p {port} (attempt {attempt}) ---"
        )
        try:
            self._process = subprocess.Popen(
                [str(binary), "-p", str(port)],
                cwd=str(self.root),
                env=self.env_vars(),
                stdout=self._log_handle,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError as exc:
            raise ProbeEnvError(f"failed to spawn {binary}: {exc}") from exc

    def _log_marker(self, text: str) -> None:
        if self._log_handle is not None:
            self._log_handle.write(f"{text}\n".encode())
            self._log_handle.flush()

    def _close_log(self) -> None:
        if self._log_handle is not None:
            try:
                self._log_handle.flush()
                self._log_handle.close()
            finally:
                self._log_handle = None

    # ── 属性 ────────────────────────────────────────────────

    @property
    def gateway_bin(self) -> Path | None:
        """解析出的网关可执行文件（未启动时为 None）。"""
        return self._gateway_bin

    @property
    def port(self) -> int | None:
        """网关端口（未启动时为 None）。"""
        return self._port

    @property
    def gateway_url(self) -> str:
        return f"http://{DEFAULT_HOST}:{self._port or 0}"

    @property
    def provider_url(self) -> str:
        return self.provider.url

    @property
    def process(self) -> subprocess.Popen[bytes] | None:
        return self._process

    @property
    def shutdown_api_key(self) -> str | None:
        """关闭自己的网关子进程时要带的 key（auth 段里挑一把非 tool_runtime 的）。

        ``/api/shutdown`` 不在 ``tool_runtime`` 的 allowlist 里：auth 打开时必须带
        一把 admin 语义的 key 才能优雅关停，否则只能等 ``terminate()`` 兜底（正确
        但慢 5s）。场景没给 auth / 没给 keys / 只给了 tool_runtime key 时返回
        None——关停退化为 terminate，不影响正确性。
        """
        keys = (self.auth or {}).get("keys")
        if not isinstance(keys, Sequence):
            return None
        fallback: str | None = None
        for entry in keys:
            if not isinstance(entry, Mapping):
                continue
            key = entry.get("key")
            if not isinstance(key, str) or not key:
                continue
            if entry.get("role", "admin") != "tool_runtime":
                return key
            fallback = fallback or key
        return fallback

    def session_dir(self, session_id: str) -> Path:
        """会话持久目录 ``<sessions>/<session_id>/``（history.jsonl 所在）。"""
        return self.sessions_path / session_id

    def log_tail(self, max_lines: int = LOG_TAIL_LINES) -> str:
        return read_log_tail(self.log_path, max_lines=max_lines)

    def register(self, model: str, script: Script) -> None:
        """注册剧本（转发给假 Provider 的注册表）。"""
        self.provider.register(model, script)

    # ── 停止 ────────────────────────────────────────────────

    async def stop(self) -> None:
        """优雅停止：``/api/shutdown`` → 等待 → ``terminate`` → ``kill``。

        幂等且不抛——失败场景的 teardown 也必须能安全调用。

        ``_stopped`` 是**单向闸门**：一旦停止，同一个 env 实例就报废了——
        ``stop()`` 之后再次 ``stop()`` 直接返回，``start()`` 也不支持在同一实例上
        重启（``ProbeEnv`` 的端口 / 子进程 / 假 Provider 状态都不是可重置的）。
        需要新环境就新建一个 ``ProbeEnv`` / ``Probe``（fixture 每场景一个，正是这条
        约定的用法）。
        """
        if self._stopped:
            return
        self._stopped = True
        try:
            if self._process is not None and self._process.poll() is None:
                await self._request_shutdown()
            await self._terminate_process()
            if self.provider.started:
                await self.provider.stop()
        finally:
            self._close_log()

    async def _request_shutdown(self) -> None:
        """POST /api/shutdown（best effort：连接已被回收时静默继续走 terminate）。

        auth 打开时该端点也受鉴权保护（不在 tool_runtime allowlist 里）——用
        :attr:`shutdown_api_key`（场景写进 ``auth.keys`` 的那把）关自己的子进程，
        否则会拿到 401、白白走 5s 的 terminate 兜底。
        """
        url = f"{self.gateway_url}/api/shutdown"
        key = self.shutdown_api_key
        headers = {"Authorization": f"Bearer {key}"} if key else None
        try:
            async with httpx.AsyncClient(
                timeout=HEALTH_REQUEST_TIMEOUT, trust_env=False
            ) as client:
                await client.post(url, json={}, headers=headers)
        except httpx.HTTPError as exc:
            _log.debug("probe shutdown request failed (%s): %s", url, exc)

    async def _terminate_process(self) -> None:
        process = self._process
        if process is None:
            return
        if process.poll() is None:
            try:
                await asyncio.to_thread(process.wait, timeout=DEFAULT_SHUTDOWN_WAIT)
            except subprocess.TimeoutExpired:
                self._log_marker("--- probe: terminate() ---")
                process.terminate()
                try:
                    await asyncio.to_thread(process.wait, timeout=DEFAULT_SHUTDOWN_WAIT)
                except subprocess.TimeoutExpired:
                    self._log_marker("--- probe: kill() ---")
                    process.kill()
                    try:
                        await asyncio.to_thread(
                            process.wait, timeout=DEFAULT_SHUTDOWN_WAIT
                        )
                    except subprocess.TimeoutExpired:
                        _log.warning("gateway process did not exit after kill()")
        self._log_marker(f"--- probe: gateway exited (code={process.returncode}) ---")
        self._process = None

    async def __aenter__(self) -> ProbeEnv:
        if self._process is None:
            await self.start_provider()
            await self.start_gateway()
        return self

    async def __aexit__(self, *_: Any) -> None:
        await self.stop()
