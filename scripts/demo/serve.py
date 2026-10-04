#!/usr/bin/env python3
"""剧情回放的假 Provider：按剧本吐工具调用，驱动一次真实的任务执行。

只有模型输出是假的（``wing_probe.provider``）：网关、agent 循环、TUI 和每一次工具
调用都是真的——``Read`` / ``Grep`` / ``Edit`` / ``Bash`` 在 ``workspace/`` 的副本上
真跑。接线（隔离 ``$WING_HOME``、网关子进程、收摊）走 `env.py`。

    uv run python scripts/demo/serve.py          # 起 provider + 网关，直到 SIGINT/SIGTERM
    # 录制：uv run python scripts/demo/record.py

环境变量见 `env.py`（``DEMO_ROOT`` / ``DEMO_WING_HOME`` / ``DEMO_WORKSPACE``）。
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import env  # 先 import env：它把 libs/wing-probe 挂上 sys.path
from wing_probe.provider.server import FakeProvider
from wing_probe.provider.script import Script, ToolCall, Turn, Usage

# ── 场景常量 ────────────────────────────────────────────────────
#: 状态栏里显示的那一行（``Wing · <model> · <provider> think:<effort>``）。
MODEL = "sonnet-4.5"
PROVIDER_NAME = "anthropic"
THINK = "high"

WS_SRC = Path(__file__).resolve().parent / "workspace"

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
        if not (env.WS_RUN / name).is_file():
            raise SystemExit(f"[demo] workspace copy is missing {name}")


async def main() -> int:
    env.copy_workspace(WS_SRC)
    env.clean_sessions()
    verify_script()
    env.say(f"[demo] workspace → {env.WS_RUN}")

    provider = FakeProvider()
    provider.scripts.register(MODEL, build_script())
    await provider.start()
    env.say(f"[demo] fake provider → {provider.base_url}")

    gateway = env.Gateway(
        provider.base_url,
        provider_name=PROVIDER_NAME,
        model=MODEL,
        system_prompt=(
            "You are wing, a coding agent working in the user's workspace. "
            "Keep answers short and concrete."
        ),
    )
    await gateway.start()
    env.say(f"[demo] config → {gateway.config_path}")
    env.say(
        f"[demo] gateway → http://127.0.0.1:{gateway.port} (pid {gateway.proc.pid})"
    )
    env.say(env.ready_line(env.WS_RUN, gateway.port))
    try:
        await env.run_until_signal()
    finally:
        env.say("[demo] shutting down")
        await provider.stop()
        gateway.stop()
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
