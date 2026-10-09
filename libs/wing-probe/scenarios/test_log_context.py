"""后端日志关联上下文场景（session / request 标注 + 包内相对路径）。

覆盖的断言点：

- ``test_turn_logs_carry_correlation_and_relative_path``：真网关跑一个文本轮，
  当日日志里 turn 链路（provider / react_loop）的每行都带
  ``[<session_id> <request_id>]`` 关联段——id 从协程上下文读取，与事件流拿到
  的 session_id / request_id 逐字一致；
- 同一批行的源码路径字段是 ``wing/...`` 相对形态，绝不出现绝对路径
  （回归：旧 root 计算让包内文件整体退化成 site-packages / 仓库长路径）；
- 守护进程 stdout 落盘 gateway.log（非 tty）不再混入 ANSI 色码；
- ``test_compact_and_release_logs_carry_session``：显式绑定（compact 端点 /
  逐出拆解）的日志同样带 ``[<session_id>]``——没有请求来源的路径不该丢失归属。

日志行格式的契约见 ``docs/dev/config-logging.md``「日志策略」。
"""

from __future__ import annotations

import asyncio
import re
import time
from pathlib import Path

import pytest

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间不共享）。
LOG_MODEL = "probe/log-context"
COMPACT_MODEL = "probe/log-context-compact"

#: 压缩产物（假 Provider 的压缩调用返回带 ``<summary>`` 的文本）。
SUMMARY_TEXT = "Task: answer the user. State: one turn done. Next: continue."

#: 行首时间戳（与 formatter 的 asctime 一致）。
_STAMP = r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}"


def _daily_log(probe: Probe) -> Path:
    """当日日志（``new.log`` 始终指向活跃文件，逐条 flush）。"""
    return probe.env.wing_home / "core" / "logs" / "new.log"


async def _wait_for_lines(probe: Probe, pattern: re.Pattern[str]) -> str:
    """轮询日志直到 ``pattern`` 命中（拆解跑在独立任务里，落日志略滞后）。"""
    deadline = time.monotonic() + 10.0
    content = ""
    while time.monotonic() < deadline:
        content = _daily_log(probe).read_text(encoding="utf-8")
        if pattern.search(content):
            return content
        await asyncio.sleep(0.05)
    raise AssertionError(f"log line never appeared; tail:\n{content[-2000:]}")


@pytest.mark.probe_env(models=[LOG_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_turn_logs_carry_correlation_and_relative_path(probe: Probe) -> None:
    """WHEN 跑完一个文本轮，THEN 日志行带关联段与相对路径。"""
    probe.register(LOG_MODEL, Turn.of(text="ok"))
    session = await probe.session(model=LOG_MODEL)

    request_id = await session.send("hello logs")
    result = await session.watch.expect("turn_result", within=15)
    assert result.data["subtype"] == "success", result.data

    content = _daily_log(probe).read_text(encoding="utf-8")

    # 完整行形态：`时间 - INFO - [<sid> <rid>] - wing/provider/... - 消息`。
    # request_id 是本轮 ClientRequest 的 id——与日志逐字一致才算"从上下文读到"。
    correlation = rf"\[{re.escape(session.session_id)} {re.escape(request_id)}\]"
    assert re.search(
        rf"^{_STAMP} - INFO - {correlation} - "
        rf"wing/provider/openai/provider\.py:\d+ - "
        rf"\[DONE\] openai_compat response header received$",
        content,
        re.MULTILINE,
    ), content

    # turn 级绑定同样生效（不止 provider 层）：react_loop 的行也带同一条关联段。
    assert re.search(
        rf"^{_STAMP} - INFO - {correlation} - wing/agent/react_loop\.py:\d+ - ",
        content,
        re.MULTILINE,
    ), content

    # 包内文件不得再渲染成绝对路径（字段边界上的 ` - /...py:N - `）。
    assert not re.search(r" - /[^\n]*\.py:\d+ - ", content), content

    # 非 tty 的守护进程 stdout 不上色（gateway.log 保持可 grep）。
    assert "\x1b[" not in probe.env.log_tail()


@pytest.mark.probe_env(models=[COMPACT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_compact_and_release_logs_carry_session(probe: Probe) -> None:
    """WHEN 显式 compact / release，THEN 两条链路各自的日志行带 ``[<sid>]``。

    只看「只带 session、不带 request id」的行：turn 的行都带 request id，
    这种形态是显式绑定生效的独有指纹（删掉任一处绑定，断言即失败）。
    """
    probe.register(
        COMPACT_MODEL,
        Turn.of(text="hello"),
        Turn.of(text=f"<summary>{SUMMARY_TEXT}</summary>"),
    )
    session = await probe.session(model=COMPACT_MODEL)
    await session.chat("hi")
    sid = re.escape(session.session_id)

    await session.compact()

    # release 需要先退订（订阅中的会话是用户工作集，release 被 409 拒绝）。
    driver = probe.driver_required
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    response = await session.release()
    assert response["released"] is True, response

    # compact：同步（非流式）LLM 调用只出现在压缩路径上——provider 行带 [sid]。
    compact_line = re.compile(
        rf"^{_STAMP} - INFO - \[{sid}\] - wing/provider/openai/provider\.py:\d+ - "
        rf"\[DONE\] openai_compat sync call$",
        re.MULTILINE,
    )
    # release：逐出拆解任务的行带 [sid]（且没有 request id——它不属于任何请求）。
    evict_line = re.compile(
        rf"^{_STAMP} - INFO - \[{sid}\] - wing/session/manager\.py:\d+ - "
        rf"Session evicted: {sid} \(released\)$",
        re.MULTILINE,
    )
    content = await _wait_for_lines(probe, evict_line)
    assert compact_line.search(content), content
    assert evict_line.search(content), content
