#!/usr/bin/env python3
"""演示环境的公共骨架：隔离的 ``$WING_HOME`` + 真网关子进程。

`serve.py`（剧情回放：假 Provider 按剧本吐工具调用）和 `stream.py`（速度演示：
按 tok/s 灌语料）都基于它——两个脚本只负责自己的假 Provider，剩下的接线
（配置生成、网关拉起、健康检查、收摊）在这里。

隔离原则：所有路径都在 ``$DEMO_ROOT``（默认 ``~/wing-demo``）下，绝不碰用户真实的
``~/.wing``。`stream.py` 的网关实例由 `serve.py` 与 `stream.py` 共用同一份
``core/config.yaml`` 形态，端口按需分配。
"""

from __future__ import annotations

import asyncio
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import yaml

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "libs" / "wing-probe"))

from wing_probe.env import (  # noqa: E402
    render_config_yaml,
    reserve_port,
    resolve_gateway_bin,
    wait_for_health,
)

#: 录制根目录。放 ``$HOME`` 下是为了让 TUI 底栏把路径缩成 ``~/…``（``/tmp`` 会展开
#: 成 ``/private/tmp``，出现在截图里很难看）。
DEMO_ROOT = Path(os.environ.get("DEMO_ROOT", Path.home() / "wing-demo")).expanduser()
#: 网关的 ``$WING_HOME`` 与工作区；工作区名出现在底栏。
WING_HOME = Path(os.environ.get("DEMO_WING_HOME", DEMO_ROOT / "home")).expanduser()
WS_RUN = Path(os.environ.get("DEMO_WORKSPACE", DEMO_ROOT / "retrykit")).expanduser()
GATEWAY_LOG = WING_HOME / "gateway.log"

#: 可见的工具集（速度演示里模型不调工具，但保持一致，配置形态才只有一处）。
TOOLS = ("Bash", "Read", "Write", "Edit", "Glob", "Grep", "TodoWrite")


def say(message: str) -> None:
    print(message, flush=True)


def write_config(
    provider_base_url: str,
    gateway_port: int,
    *,
    provider_name: str,
    model: str,
    display_name: str | None = None,
    system_prompt: str,
    tools: tuple[str, ...] = TOOLS,
    context_window_tokens: int = 256_000,
    log_level: str = "INFO",
) -> Path:
    """写 ``$WING_HOME/core/config.yaml``：provider 指向假 Provider，agent 预置。"""
    config = yaml.safe_load(
        render_config_yaml(
            provider_base_url=provider_base_url,
            gateway_port=gateway_port,
            provider_name=provider_name,
            model=model,
            tools=tools,
            system_prompt=system_prompt,
            context_window_tokens=context_window_tokens,
            log_level=log_level,
        )
    )
    config["providers"][0]["models"] = [
        {"name": model, "display_name": display_name or model}
    ]
    config["yolo"] = True  # 录制里不要确认弹窗
    path = WING_HOME / "core" / "config.yaml"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        yaml.safe_dump(config, sort_keys=False, allow_unicode=True), encoding="utf-8"
    )
    return path


def clean_sessions() -> None:
    """清掉上一次录制留下的会话：每次录制从同一张白纸开始。"""
    sessions = WING_HOME / "core" / "sessions"
    if sessions.exists():
        shutil.rmtree(sessions)


def copy_workspace(template: Path) -> None:
    """模板 → 运行目录（录制会改文件，仓库里的模板保持干净）。"""
    root = DEMO_ROOT.resolve()
    run = WS_RUN.resolve()
    if not run.is_relative_to(root) or run == root:
        raise SystemExit(f"[demo] refusing to clean {run}: not inside {root}")
    if WS_RUN.exists():
        shutil.rmtree(WS_RUN)
    WS_RUN.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(template, WS_RUN, ignore=shutil.ignore_patterns("__pycache__"))


class Gateway:
    """演示网关子进程（起/停/健康检查）。"""

    def __init__(self, provider_base_url: str, **config: object) -> None:
        self.port = reserve_port()
        self.config_path = write_config(provider_base_url, self.port, **config)  # type: ignore[arg-type]
        self.proc: subprocess.Popen[bytes] | None = None
        self._log = None

    async def start(self) -> None:
        self._log = open(GATEWAY_LOG, "wb")
        self.proc = subprocess.Popen(
            [str(resolve_gateway_bin())],
            cwd=str(REPO),
            env={
                **os.environ,
                "WING_HOME": str(WING_HOME),
                # loopback 必须绕开环境 / 系统代理，否则假 Provider 会被送进代理。
                "NO_PROXY": "127.0.0.1,localhost",
                "no_proxy": "127.0.0.1,localhost",
            },
            stdout=self._log,
            stderr=subprocess.STDOUT,
        )
        await wait_for_health(
            f"http://127.0.0.1:{self.port}/api/health",
            process=self.proc,
            log_path=GATEWAY_LOG,
        )

    def stop(self) -> None:
        if self.proc is None:
            return
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.proc = None
        if self._log is not None:
            self._log.close()
            self._log = None


async def run_until_signal() -> None:
    """等 SIGINT / SIGTERM（录制器就是用 SIGTERM 收摊的）。"""
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)
    await stop.wait()


def ready_line(workspace: Path, port: int) -> str:
    """录制器解析这一行（见 record.py 的 ready 正则）。"""
    return f"[demo] ready WING_HOME={WING_HOME} WING_WORKSPACE={workspace} WING_GATEWAY_PORT={port}"


def sleep_until(deadline: float) -> None:
    """睡到 ``deadline``（perf_counter 口径）；已经过了就直接返回。

    假 Provider 的发射节奏靠它自校正：不是"每帧 sleep 固定值"，而是"睡到下一帧
    的计划时刻"——否则 sleep 粒度与处理开销会一路吃掉速率，3000 tok/s 会变成 2400。
    """
    delay = deadline - time.perf_counter()
    if delay > 0:
        time.sleep(delay)
