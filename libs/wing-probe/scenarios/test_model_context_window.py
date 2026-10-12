"""模型级上下文窗口 / 压缩阈值 —— 跟随当前模型（含运行期换模型）。

模型声明（``providers[].models[]``）是这两个值的唯一配置来源：

- ``/api/models`` 逐条目下发**生效值**（显式声明原样；未声明 = 声明层默认
  256000 / 50000）；
- 会话创建（含 create override 选中的模型）与 ``update {model_id}`` 都把 compactor
  双阈值重绑定到**当前模型**——``/api/session/info.context_window_tokens`` 与
  ``context_stats`` 事件同刻跟随；
- 压缩行为跟随模型而不是建会话时冻结：小窗口下服务端 usage 跨过 early / apply
  阈值 → 后台压缩 + pending 落盘；换到大窗口后同一份上下文**不 apply**
  （pending 留在盘上）；换回小窗口的下一轮才 apply。
"""

from __future__ import annotations

import asyncio
import time

import pytest

from wing_probe import Probe, Turn, Usage, assert_compact_transition
from wing_probe.history.view import COMPACT_PREFIX

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
SMALL_ID = "probe/window-small"
LARGE_ID = "probe/window-large"

#: 小窗口：early = 1000 − 400 = 600、apply = 1000——服务端 usage 1100 同时跨过两者。
SMALL_WINDOW = 1000
SMALL_KEEP = 400
#: 大窗口：同一份上下文远低于它的两个阈值。
LARGE_WINDOW = 100_000
LARGE_KEEP = 20_000

#: 模板模型（``ProbeEnv`` 的默认 model）不声明窗口 → 生效值 = 声明层默认。
TEMPLATE_ID = "probe/default"
DEFAULT_WINDOW = 256_000
DEFAULT_KEEP = 50_000

#: 声明原文（显式窗口由场景负责——探针旋钮保持默认时不替模型补键）。
SMALL_SPEC = {
    "name": SMALL_ID,
    "context_window_tokens": SMALL_WINDOW,
    "keep_recent_tokens": SMALL_KEEP,
}
LARGE_SPEC = {
    "name": LARGE_ID,
    "context_window_tokens": LARGE_WINDOW,
    "keep_recent_tokens": LARGE_KEEP,
}

SERVER_TOKENS = 1100

SUMMARY_TEXT = "window switch summary"
COMPACT_CONTENT = f"{COMPACT_PREFIX} {SUMMARY_TEXT}"
SUMMARY_LOOKING_TEXT = f"<summary>{SUMMARY_TEXT}</summary>"

PENDING_KEY = "pending_compact"
POLL_INTERVAL = 0.05
PENDING_DEADLINE = 20.0


def _usage() -> Usage:
    return Usage(prompt_tokens=SERVER_TOKENS)


def _small_script() -> tuple[Turn, ...]:
    """小窗口剧本：主调用与后台压缩并发，第 2/3 轮写同文本让结论与到达顺序无关。"""
    return (
        Turn.of(text="reply one", usage=_usage()),
        Turn.of(text=SUMMARY_LOOKING_TEXT, usage=_usage()),
        Turn.of(text=SUMMARY_LOOKING_TEXT, usage=_usage()),
        Turn.of(text="post compact reply", usage=_usage()),
    )


async def _wait_pending(probe: Probe, session: str) -> dict:
    """轮询到 pending compact 已落盘（后台 task 的完成时刻不可预测）。"""
    observed: dict | None = None
    expires = time.monotonic() + PENDING_DEADLINE
    while time.monotonic() < expires:
        observed = probe.history(session).aux(PENDING_KEY)
        if observed is not None:
            return observed
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"no pending compact under key {PENDING_KEY!r} within {PENDING_DEADLINE:.0f}s; "
        f"requests: {probe.requests.summary()}"
    )


@pytest.mark.probe_env(models=[SMALL_SPEC, LARGE_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_model_window_follows_current_model(probe: Probe) -> None:
    probe.register(SMALL_ID, *_small_script())
    probe.register(LARGE_ID, Turn.of(text="post large reply", usage=_usage()))
    http = probe.driver_required.http

    # ① 目录：每条声明携带生效窗口（显式原样；未声明 = 声明层默认）。
    catalog = await http.get_models()
    entries = {m["id"]: m for m in catalog["providers"][0]["models"]}
    assert (
        entries[SMALL_ID]["context_window_tokens"],
        entries[SMALL_ID]["keep_recent_tokens"],
    ) == (SMALL_WINDOW, SMALL_KEEP), entries[SMALL_ID]
    assert (
        entries[LARGE_ID]["context_window_tokens"],
        entries[LARGE_ID]["keep_recent_tokens"],
    ) == (LARGE_WINDOW, LARGE_KEEP), entries[LARGE_ID]
    assert (
        entries[TEMPLATE_ID]["context_window_tokens"],
        entries[TEMPLATE_ID]["keep_recent_tokens"],
    ) == (DEFAULT_WINDOW, DEFAULT_KEEP), entries[TEMPLATE_ID]

    # ② 建会话（create override 选 SMALL）：info 与 context_stats 都给小窗口。
    session = await probe.session(model=SMALL_ID)
    info = await http.get_session_info(session.session_id)
    assert info["context_window_tokens"] == SMALL_WINDOW, info

    await session.chat("alpha")
    stats = session.watch.events(type="context_stats")
    assert stats[-1].data["context_window_tokens"] == SMALL_WINDOW, stats[-1].data

    # ③ 小窗口下服务端 usage 跨过 early → 后台压缩 → pending 落盘（链未变）。
    await session.chat("beta")
    pending = await _wait_pending(probe, session.session_id)
    assert pending["compact_content"] == COMPACT_CONTENT, pending

    # ④ 换到大窗口：info 跟随；pending 已够 apply 但大窗口不触发——链保持原样。
    await http.update_session(session.session_id, model_id=LARGE_ID)
    info = await http.get_session_info(session.session_id)
    assert info["model_id"] == LARGE_ID, info
    assert info["context_window_tokens"] == LARGE_WINDOW, info

    await session.chat("gamma")
    assert probe.history(session).aux(PENDING_KEY) is not None
    stats = session.watch.events(type="context_stats")
    assert stats[-1].data["context_window_tokens"] == LARGE_WINDOW, stats[-1].data
    large_context = probe.request(LARGE_ID, 0).context()
    assert [m.content for m in large_context.messages if m.role == "user"] == [
        "alpha",
        "beta",
        "gamma",
    ], large_context.describe()
    assert COMPACT_CONTENT not in large_context.describe(), large_context.describe()

    # ⑤ 换回小窗口：下一轮 apply（阈值真的跟随当前模型，不是建会话时冻结的值）。
    await http.update_session(session.session_id, model_id=SMALL_ID)
    info = await http.get_session_info(session.session_id)
    assert info["context_window_tokens"] == SMALL_WINDOW, info

    before = probe.history(session)
    await session.chat("delta")
    after = probe.history(session)
    material = assert_compact_transition(before, after, allow_trailing=True)
    assert material["compact_uuid"] is not None
    assert after.by_uuid[material["compact_uuid"]]["content"] == COMPACT_CONTENT
    assert after.aux(PENDING_KEY) is None
    context = probe.request(SMALL_ID, 3).context()
    assert context.messages[0].content == COMPACT_CONTENT, context.describe()
    session.watch.assert_never("error")
