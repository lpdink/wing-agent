#!/usr/bin/env python3
"""Boot a scripted demo session: fake provider + real gateway.

Only the model output is scripted (``wing_probe.provider``): the gateway, the
agent loop, the TUI and every tool call are the real thing — ``Read`` / ``Grep``
/ ``Edit`` / ``Bash`` run against a scratch copy of ``workspace/``.

    uv run python scripts/demo/serve.py          # stays up until SIGINT/SIGTERM

Environment:
    DEMO_ROOT        scratch root            (default: ``/tmp/wing-demo``)
    DEMO_WING_HOME   gateway home            (default: ``$DEMO_ROOT/home``)
    DEMO_WORKSPACE   workspace copy          (default: ``$DEMO_ROOT/workspace``)
    WING_GATEWAY_BIN override the gateway binary
"""

from __future__ import annotations

import asyncio
import os
import shutil
import signal
import subprocess
import sys
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
from wing_probe.provider.server import FakeProvider  # noqa: E402
from wing_probe.provider.script import Script, ToolCall, Turn, Usage  # noqa: E402

# ── 场景常量 ────────────────────────────────────────────────────
#: 状态栏里显示的那一行（``Wing · <model> · <provider> think:<effort>``）。
MODEL = "sonnet-4.5"
PROVIDER_NAME = "anthropic"
THINK = "high"

DEMO_ROOT = Path(os.environ.get("DEMO_ROOT", Path.home() / "wing-demo")).expanduser()
WS_SRC = Path(__file__).resolve().parent / "workspace"
#: 录制工作区。放 ``$HOME`` 下是为了让 TUI 底栏把路径缩成 ``~/…``（``/tmp`` 会
#: 展开成 ``/private/tmp``，出现在截图里很难看）。
WS_RUN = Path(os.environ.get("DEMO_WORKSPACE", DEMO_ROOT / "retrykit")).expanduser()
WING_HOME = Path(os.environ.get("DEMO_WING_HOME", DEMO_ROOT / "home")).expanduser()
GATEWAY_LOG = WING_HOME / "gateway.log"

TOOLS = ("Bash", "Read", "Write", "Edit", "Glob", "Grep", "TodoWrite")

# ── 节奏：分片粒度（字符）与片间延迟（秒）───────────────────────
# 目标观感 ≈ 120~150 字符/秒（"快模型"的手感），整段 ~20s。
FAST = {"chunk": 6, "delay": 0.035}  # 思考 + 正文
ARGS = {"chunk": 99, "delay": 0.055}  # 工具参数：靠 cut 点分帧，见 cuts()

# ── 剧本用到的代码片段（与 workspace/client.py 逐字对应）────────
FETCH_OLD = '''def fetch(url: str, *, timeout: float = 5.0) -> str:
    """GET *url* and return the body as text."""
    with urllib.request.urlopen(url, timeout=timeout) as resp:
        return resp.read().decode("utf-8")
'''

FETCH_NEW = '''def fetch(
    url: str,
    *,
    timeout: float = 5.0,
    attempts: int = DEFAULT_ATTEMPTS,
    backoff: float = DEFAULT_BACKOFF,
) -> str:
    """GET *url*, retrying transient failures with exponential backoff."""
    last_error: Exception | None = None
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(url, timeout=timeout) as resp:
                return resp.read().decode("utf-8")
        except (urllib.error.URLError, TimeoutError) as exc:
            last_error = exc
            if attempt + 1 < attempts:
                _sleep(backoff * 2**attempt)
    raise TransientError(f"{url} unavailable after {attempts} attempts") from last_error
'''

#: 三件任务，按序推进（第 3 项的名字在收尾时复用）。
T_RUN = "Run the suite to see what fails"
T_ADD = "Add retry + exponential backoff to client.fetch"
T_RERUN = "Re-run the suite"

RUN_RED = "python3 -m unittest 2>&1 | tail -n 6"
RUN_GREEN = "python3 -m unittest -v"


def cuts(text: str, every: int) -> list[int]:
    """把 JSON 参数切成 ``every`` 字符一段的分帧点。

    避开转义序列中间（``\\n`` / ``\\"``）：落点前一个字符是反斜杠就往后退一格，
    否则前端会在半截转义上做局部解析。
    """
    points = []
    pos = every
    while pos < len(text):
        while pos > 0 and text[pos - 1] == "\\":
            pos -= 1
        if pos > 0 and (not points or pos > points[-1]):
            points.append(pos)
        pos += every
    return points


def call(name: str, arguments: dict, *, every: int | None = None) -> ToolCall:
    """带分帧点的工具调用：``every`` = 参数每多少字符吐一帧。"""
    if every is None:
        return ToolCall(name, arguments)
    import json

    payload = json.dumps(arguments, ensure_ascii=False)
    return ToolCall(name, payload, cut=cuts(payload, every))


def todo(items: list[tuple[str, str]]) -> ToolCall:
    """TodoWrite 调用：``[(content, status)]``。"""
    payload = []
    for content, status in items:
        item: dict[str, object] = {"content": content, "status": status}
        if status == "in_progress":
            item["activeForm"] = content.replace("Add ", "Adding ").replace(
                "Run ", "Running"
            )
        payload.append(item)
    return call("TodoWrite", {"todos": payload}, every=90)


class DemoScript(Script):
    """剧本 + 一个给录制器看的信号：最后一轮被消费时报出来。

    录制器拿它当「该收尾了」的触发点（再等会话回 idle，然后停），比按固定
    时长猜要稳：真跑一段剧本要多久取决于工具耗时。
    """

    def consume(self, *, model: str = "") -> tuple[Turn, int]:
        turn, index = super().consume(model=model)
        if self.remaining == 0:
            print("[demo] script exhausted", flush=True)
        return turn, index


def build_script() -> Script:
    """剧本：7 轮工具 + 1 轮收尾（每轮 = 一次请求 / 一次模型输出）。"""
    return DemoScript(
        # 1) 读完清单 → 建任务列表
        Turn.of(
            thinking=(
                "Three tests pin the behaviour: the body comes back unchanged, "
                "transient failures are retried with doubling backoff, and the last "
                "attempt re-raises as TransientError."
            ),
            text=(
                "Two of the three tests are red — `fetch` never retries. I'll add "
                "attempts + exponential backoff, keeping the `_sleep` seam so the "
                "tests stay deterministic."
            ),
            tool_calls=[
                todo([(T_RUN, "in_progress"), (T_ADD, "pending"), (T_RERUN, "pending")])
            ],
            usage=Usage(prompt_tokens=2_640, completion_tokens=286),
            **FAST,
        ),
        # 2) 先跑一遍：红
        Turn.of(
            thinking="Run it before touching anything — the failures are the spec.",
            tool_calls=[call("Bash", {"command": RUN_RED, "timeout": 60}, every=40)],
            usage=Usage(prompt_tokens=3_180, cached_tokens=2_640, completion_tokens=94),
            **FAST,
        ),
        # 3) 勾掉第一项
        Turn.of(
            tool_calls=[
                todo(
                    [(T_RUN, "completed"), (T_ADD, "in_progress"), (T_RERUN, "pending")]
                )
            ],
            usage=Usage(prompt_tokens=3_420, cached_tokens=3_180, completion_tokens=72),
            **ARGS,
        ),
        # 4) 读文件
        Turn.of(
            thinking="Both failures come straight out of urlopen raising URLError.",
            tool_calls=[call("Read", {"path": "client.py"}, every=30)],
            usage=Usage(prompt_tokens=3_640, cached_tokens=3_420, completion_tokens=78),
            **FAST,
        ),
        # 5) 确认没有第二处调用点
        Turn.of(
            tool_calls=[
                call("Grep", {"pattern": "_sleep", "output_mode": "content"}, every=30)
            ],
            usage=Usage(prompt_tokens=3_860, cached_tokens=3_640, completion_tokens=64),
            **ARGS,
        ),
        # 6) 改文件 → diff 逐行长出来
        Turn.of(
            thinking=(
                "Keep the diff small: wrap the request in a loop, reuse the _sleep "
                "indirection, chain the last error into TransientError."
            ),
            tool_calls=[
                call(
                    "Edit",
                    {
                        "path": "client.py",
                        "old_string": FETCH_OLD,
                        "new_string": FETCH_NEW,
                    },
                    every=48,
                )
            ],
            usage=Usage(
                prompt_tokens=4_120, cached_tokens=3_860, completion_tokens=268
            ),
            **FAST,
        ),
        # 7) 再跑一遍：绿
        Turn.of(
            tool_calls=[call("Bash", {"command": RUN_GREEN, "timeout": 60}, every=40)],
            usage=Usage(prompt_tokens=4_460, cached_tokens=4_120, completion_tokens=86),
            **FAST,
        ),
        # 8) 收尾：勾完 + 结论
        Turn.of(
            tool_calls=[
                todo(
                    [(T_RUN, "completed"), (T_ADD, "completed"), (T_RERUN, "completed")]
                )
            ],
            usage=Usage(prompt_tokens=4_620, cached_tokens=4_460, completion_tokens=64),
            **ARGS,
        ),
        Turn.of(
            text=(
                "All three tests pass.\n\n"
                "- `fetch` retries `URLError` / `TimeoutError` up to `DEFAULT_ATTEMPTS`, "
                "sleeping `0.5 * 2**n` between tries.\n"
                "- The last failure is re-raised as `TransientError` (`from` keeps the "
                "cause), so callers see one error type.\n"
                "- Both knobs are per-call: `fetch(url, attempts=5, backoff=0.1)`."
            ),
            usage=Usage(
                prompt_tokens=4_780, cached_tokens=4_620, completion_tokens=212
            ),
            **FAST,
        ),
    )


def verify_script() -> None:
    """剧本与工作区模板必须对得上——对不上的话录到一半 Edit 会失败。

    这类"改了一边忘了另一边"（比如把 `client.py` 里的函数体改了）不会有类型错误，
    只会在录制时变成一张红卡片，所以在这里硬校验。
    """
    source = (WS_SRC / "client.py").read_text(encoding="utf-8")
    if FETCH_OLD not in source:
        raise SystemExit(
            "[demo] script/workspace drift: FETCH_OLD is not in workspace/client.py.\n"
            "  update FETCH_OLD/FETCH_NEW in serve.py (or the template) so the Edit matches"
        )
    if FETCH_NEW.strip() and FETCH_NEW in source:
        raise SystemExit(
            "[demo] script/workspace drift: workspace/client.py already contains FETCH_NEW.\n"
            "  the demo edits a *pre-edit* workspace; restore the template"
        )
    for name in ("client.py", "test_client.py"):
        if not (WS_RUN / name).is_file():
            raise SystemExit(f"[demo] workspace copy is missing {name}")


def prepare_workspace() -> None:
    """模板 → 运行目录，并清掉上一次录制的会话（每次录制从同一张白纸开始）。

    删之前先确认 ``WS_RUN`` 真在 ``DEMO_ROOT`` 里：这个目录是我们自己建的，
    但 ``DEMO_*`` 都是可覆盖的环境变量，误指到别人的目录上就是数据丢失。
    """
    root = DEMO_ROOT.resolve()
    run = WS_RUN.resolve()
    if not run.is_relative_to(root) or run == root:
        raise SystemExit(f"[demo] refusing to clean {run}: not inside {root}")
    if WS_RUN.exists():
        shutil.rmtree(WS_RUN)
    WS_RUN.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(WS_SRC, WS_RUN, ignore=shutil.ignore_patterns("__pycache__"))
    sessions = WING_HOME / "core" / "sessions"
    if sessions.exists():
        shutil.rmtree(sessions)


def write_config(provider_base_url: str, gateway_port: int) -> Path:
    config = yaml.safe_load(
        render_config_yaml(
            provider_base_url=provider_base_url,
            gateway_port=gateway_port,
            provider_name=PROVIDER_NAME,
            model=MODEL,
            tools=TOOLS,
            system_prompt=(
                "You are wing, a coding agent working in the user's workspace. "
                "Keep answers short and concrete."
            ),
            context_window_tokens=256_000,
            log_level="INFO",
        )
    )
    config["providers"][0]["models"] = [{"name": MODEL, "display_name": MODEL}]
    config["yolo"] = True  # 录制里不要确认弹窗（Bash 直接跑）
    path = WING_HOME / "core" / "config.yaml"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        yaml.safe_dump(config, sort_keys=False, allow_unicode=True), encoding="utf-8"
    )
    return path


def say(message: str) -> None:
    print(message, flush=True)


async def main() -> int:
    prepare_workspace()
    verify_script()
    say(f"[demo] workspace → {WS_RUN}")

    provider = FakeProvider()
    provider.scripts.register(MODEL, build_script())
    await provider.start()
    say(f"[demo] fake provider → {provider.base_url}")

    gateway_port = reserve_port()
    config_path = write_config(provider.base_url, gateway_port)
    say(f"[demo] config → {config_path}")

    gateway_bin = resolve_gateway_bin()
    log = open(GATEWAY_LOG, "wb")
    proc = subprocess.Popen(
        [str(gateway_bin)],
        cwd=str(REPO),
        env={
            **os.environ,
            "WING_HOME": str(WING_HOME),
            # loopback 必须绕开环境 / 系统代理，否则假 Provider 会被送进代理。
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        },
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    try:
        await wait_for_health(
            f"http://127.0.0.1:{gateway_port}/api/health",
            process=proc,
            log_path=GATEWAY_LOG,
        )
    except Exception as exc:  # 启动失败要能一眼看出为什么
        say(f"[demo] gateway failed: {exc}")
        return 1
    say(f"[demo] gateway → http://127.0.0.1:{gateway_port} (pid {proc.pid})")
    say(
        f"[demo] ready WING_HOME={WING_HOME} WING_WORKSPACE={WS_RUN} "
        f"WING_GATEWAY_PORT={gateway_port}"
    )

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)
    await stop.wait()

    say("[demo] shutting down")
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    await provider.stop()
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
