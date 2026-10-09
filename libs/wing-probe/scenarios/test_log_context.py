"""后端日志关联上下文场景（session / request 标注 + 包内相对路径）。

覆盖的断言点：

- ``test_turn_logs_carry_correlation_and_relative_path``：真网关跑一个文本轮，
  当日日志里 turn 链路（provider / react_loop）的每行都带
  ``[<session_id> <request_id>]`` 关联段——id 从协程上下文读取，与事件流拿到
  的 session_id / request_id 逐字一致；
- 同一批行的源码路径字段是 ``wing/...`` 相对形态，绝不出现绝对路径
  （回归：旧 root 计算让包内文件整体退化成 site-packages / 仓库长路径）；
- 守护进程 stdout 落盘 gateway.log（非 tty）不再混入 ANSI 色码。

日志行格式的契约见 ``docs/dev/config-logging.md``「日志策略」。
"""

from __future__ import annotations

import re

import pytest

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间不共享）。
LOG_MODEL = "probe/log-context"


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

    content = (probe.env.wing_home / "core" / "logs" / "new.log").read_text(
        encoding="utf-8"
    )

    # 完整行形态：`时间 - INFO - [<sid> <rid>] - wing/provider/... - 消息`。
    # request_id 是本轮 ClientRequest 的 id——与日志逐字一致才算"从上下文读到"。
    correlation = rf"\[{re.escape(session.session_id)} {re.escape(request_id)}\]"
    stamp = r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}"
    assert re.search(
        rf"^{stamp} - INFO - {correlation} - "
        rf"wing/provider/openai/provider\.py:\d+ - "
        rf"\[DONE\] openai_compat response header received$",
        content,
        re.MULTILINE,
    ), content

    # turn 级绑定同样生效（不止 provider 层）：react_loop 的行也带同一条关联段。
    assert re.search(
        rf"^{stamp} - INFO - {correlation} - wing/agent/react_loop\.py:\d+ - ",
        content,
        re.MULTILINE,
    ), content

    # 包内文件不得再渲染成绝对路径（字段边界上的 ` - /...py:N - `）。
    assert not re.search(r" - /[^\n]*\.py:\d+ - ", content), content

    # 非 tty 的守护进程 stdout 不上色（gateway.log 保持可 grep）。
    assert "\x1b[" not in probe.env.log_tail()
