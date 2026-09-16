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

#: 网关重试 / 退避不进首批（design D9）：探测配置显式关闭重试，保证
#: "一次请求 = 一次剧本消费"的可数性。
PROBE_MAX_RETRIES = 0


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


def render_config_yaml(
    *,
    provider_base_url: str,
    gateway_port: int,
    provider_name: str = DEFAULT_PROVIDER_NAME,
    provider_api_key: str = DEFAULT_PROVIDER_API_KEY,
    model: str = DEFAULT_PROBE_MODEL,
    tools: Sequence[str] = DEFAULT_AGENT_TOOLS,
    system_prompt: str = DEFAULT_SYSTEM_PROMPT,
    context_window_tokens: int = 256_000,
    keep_recent_tokens: int = 50_000,
    log_level: str = "INFO",
) -> str:
    """生成 probe 网关配置（providers 指向假 Provider；agents 预置 default）。

    base_url 是 OpenAI 兼容根（``http://host:port/v1``）——provider 在其后
    拼 ``/chat/completions`` 与 ``/models``。``system_prompt`` 默认非空
    （见 :data:`DEFAULT_SYSTEM_PROMPT`），空串会让 system 段从请求里消失。
    """
    config: dict[str, Any] = {
        "providers": [
            {
                "name": provider_name,
                "protocol": "openai",
                "base_url": provider_base_url.rstrip("/"),
                "api_key": provider_api_key,
                "timeout_first_chunk": 30.0,
                "timeout_total": 120.0,
                "max_retries": PROBE_MAX_RETRIES,
                "max_retry_delay": 1.0,
                "explicit_cache_mode": True,
                "extra_body": {},
            }
        ],
        "agents": [
            {
                "name": "default",
                "model": model,
                "provider": provider_name,
                "default": True,
                "system_prompt": system_prompt,
                "tools": list(tools),
                "context_window_tokens": context_window_tokens,
                "keep_recent_tokens": keep_recent_tokens,
                "skills": [],
                "rules": [],
            }
        ],
        "hooks": [],
        "safe_command_patterns": [],
        "yolo": False,
        "log": {"level": log_level},
        "gateway": {
            "host": DEFAULT_HOST,
            "port": gateway_port,
            "auth": {"enabled": False},
        },
        "commands": {"paths": []},
    }
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
    async with httpx.AsyncClient(timeout=timeout_ctx) as client:
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
        tools: Sequence[str] = DEFAULT_AGENT_TOOLS,
        system_prompt: str = DEFAULT_SYSTEM_PROMPT,
        env_overrides: Mapping[str, str] | None = None,
    ) -> None:
        self.root = Path(root).expanduser().resolve()
        self.wing_home = self.root / "wing_home"
        self.sessions_path = self.root / "sessions"
        self.artifacts_path = self.root / "artifacts"
        self.log_path = self.root / "gateway.log"
        self.config_path = self.wing_home / "core" / "config.yaml"
        self.model = model
        self.tools = tuple(tools)
        self.system_prompt = system_prompt
        """默认模板的 system prompt（非空；场景可与请求里的 system 段对照）。"""

        self.provider = FakeProvider(host=DEFAULT_HOST)
        """进程内假 Provider（注册剧本 / 读请求留档）。"""

        self._gateway_bin_override = (
            Path(gateway_bin).expanduser() if gateway_bin is not None else None
        )
        self._health_timeout = health_timeout
        self._gateway_attempts = gateway_attempts
        self._env_overrides = dict(env_overrides or {})
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
            write_config(
                self.wing_home,
                render_config_yaml(
                    provider_base_url=self.provider.base_url,
                    gateway_port=port,
                    model=self.model,
                    tools=self.tools,
                    system_prompt=self.system_prompt,
                ),
            )
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

    def startup_report(self, binary: Path | None = None) -> str:
        """启动上下文（解析结果 / 命令 / 隔离目录 / 假 Provider），失败报告用。"""
        resolved = binary or self._gateway_bin
        return "\n".join(
            [
                "--- probe startup context ---",
                f"  binary: {resolved}",
                f"  command: {' '.join(self.command())}",
                f"  WING_HOME: {self.wing_home}",
                f"  WING_SESSIONS_PATH: {self.sessions_path}",
                f"  provider: {self.provider_url}",
                f"  health timeout: {self._health_timeout:.1f}s",
                f"  log: {self.log_path}",
            ]
        )

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
        """网关子进程环境：显式指向 tmp（防外部环境污染，design D7）。"""
        env = dict(os.environ)
        env["WING_HOME"] = str(self.wing_home)
        env["WING_SESSIONS_PATH"] = str(self.sessions_path)
        env["PYTHONUNBUFFERED"] = "1"
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
        """POST /api/shutdown（best effort：连接已被回收时静默继续走 terminate）。"""
        url = f"{self.gateway_url}/api/shutdown"
        try:
            async with httpx.AsyncClient(timeout=HEALTH_REQUEST_TIMEOUT) as client:
                await client.post(url, json={})
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
