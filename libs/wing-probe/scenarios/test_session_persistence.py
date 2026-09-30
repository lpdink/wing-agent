"""会话状态持久化场景：fork / 逐出水合后请求前缀不变（KV cache 红线）。

**前缀身份**（缓存命中的前提）不只有消息：``system`` 段、``tools`` 声明与
影响服务端处理的 body 开关（``enable_thinking`` / ``preserve_thinking`` /
``reasoning_effort``）都参与"前缀"。会话重建（fork 子会话构造、逐出后按需
水合 resume）时它们必须逐字节复现——这正是本轮修复的 bug：hook 注入的
追加系统提示词此前只存在于内存，fork / resume 重建 CM 即丢失，system 段
从第 0 个 token 起就与重建前不同，整个上下文无法命中缓存。

覆盖：

- ``test_hook_injected_append_survives_fork_and_rehydrate``：``before_session_start``
  hook（经 ``POST /api/system/reload`` 加载，模拟 ~/.wing/hooks）注入的内容
  在 fork 与逐出水合后仍在 system 段里，且请求前缀逐字节一致；
- ``test_dynamic_state_and_overrides_survive_rehydrate``：创建 override
  （system_prompt / append_system_prompt / tools / max_turns / effort / yolo）
  与运行时开关（thinking）在水合后依旧生效，请求 body 逐字段一致。

环境旋钮：``@pytest.mark.probe_env(hooks=["hooks/*.py"])``——相对路径按网关
进程 cwd（``env.root``）解析，钩子文件由场景在 reload 前写入。
"""

from __future__ import annotations

import asyncio
import time

import pytest

from wing_probe import (
    Driver,
    LoggedRequest,
    Probe,
    Session,
    Turn,
    assert_fork_of,
)

#: hook 注入的环境标记（断言 system 段时逐字节对账的锚点）。
HOOK_MARKER = "<probe-env>injected-at-create</probe-env>"

#: 场景写入 ``<env.root>/hooks/`` 的 hook 源（模拟用户的 workspace_env_inject）。
HOOK_SOURCE = '''"""probe hook：before_session_start 注入环境标记。"""

from wing.hook_registry import hooks


@hooks.on("before_session_start")
def inject_env_marker(session, **ctx):
    session.context_manager.append_to_system_prompt(
        "<probe-env>injected-at-create</probe-env>"
    )
'''

HOOK_MODEL = "probe/persist-hook"
STATE_MODEL = "probe/persist-state"


def _install_hook(probe: Probe) -> None:
    """把 hook 文件写进 ``<env.root>/hooks/``（reload 前调用）。"""
    hook_dir = probe.env.root / "hooks"
    hook_dir.mkdir(parents=True, exist_ok=True)
    (hook_dir / "probe_inject.py").write_text(HOOK_SOURCE, encoding="utf-8")


async def _evict(probe: Probe, session_id: str) -> None:
    """显式逐出：断开订阅 → ``POST /api/session/release``（幂等断言 released）。"""
    driver = probe.driver_required
    await driver.http.unsubscribe(session_id, driver.client_id)
    payload = await driver.http.request(
        "POST", "/api/session/release", body={"session_id": session_id}
    )
    assert payload == {"ok": True, "released": True, "detail": "released"}, payload


async def _hydrate(probe: Probe, session_id: str) -> Session:
    """按需水合（resume 重建 agent）并重新订阅。

    重新订阅是显式的：``Driver.attach`` 对已挂载句柄复用时间线、**不重复发起
    订阅**（假定路由仍在）——逐出前我们主动断开了订阅，因此这里要接回来，
    否则水合后的事件不会进入时间线（见 test_session_eviction 的同款做法）。
    """
    driver = probe.driver_required
    session = await probe.resume(session_id)
    await driver.http.subscribe(session_id, driver.client_id)
    return session


async def _wait_inactive(probe: Probe, session_id: str) -> None:
    """轮询 ``/api/session/list`` 直到该会话转 ``inactive``。"""
    driver: Driver = probe.driver_required
    deadline = time.monotonic() + 20.0
    while True:
        payload = await driver.http.request("GET", "/api/session/list")
        statuses = {
            entry.get("id"): entry.get("status")
            for entry in payload.get("sessions", [])
        }
        if statuses.get(session_id) == "inactive":
            return
        if time.monotonic() >= deadline:
            raise AssertionError(
                f"session {session_id} did not become inactive "
                f"(last status={statuses.get(session_id)!r})"
            )
        await asyncio.sleep(0.2)


def _prefix_flags(body: dict) -> dict:
    """请求里影响服务端处理/缓存身份的开关字段（对账用）。

    键取 OpenAI 兼容协议形态（probe 假 Provider 走的就是它）；anthropic 协议的
    对应物是 ``thinking``（extra_body 透传），当前 probe 环境不产生该协议请求。
    """
    return {
        key: body.get(key)
        for key in ("enable_thinking", "preserve_thinking", "reasoning_effort")
    }


def _assert_prefix_identity(
    source_req: LoggedRequest, actual_req: LoggedRequest, *, shared: int
) -> None:
    """共享前缀逐项对账：system / tools 声明 / 处理开关 / 前 ``shared`` 条消息。

    消息按 ``ContextView`` 的 role + 归一化 content 比较（不逐字节比 JSON）：
    ``cache_control`` 标记会把**各自请求的最后一条**消息的 content 序列化成
    块数组——那是标记位置差异，不是 token 差异（前缀缓存比较的是文本）。
    """
    sbody, cbody = source_req.body, actual_req.body
    assert cbody["messages"][0] == sbody["messages"][0], "system 段必须逐字节一致"
    assert cbody["tools"] == sbody["tools"], "tools 声明必须一致"
    assert _prefix_flags(cbody) == _prefix_flags(sbody), "处理开关必须一致"
    sctx, cctx = source_req.context(), actual_req.context()
    for position in range(shared):
        left, right = sctx.messages[position], cctx.messages[position]
        assert (left.role, left.content) == (right.role, right.content), (
            position,
            left.summary(),
            right.summary(),
        )


@pytest.mark.probe_env(hooks=["hooks/*.py"])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_hook_injected_append_survives_fork_and_rehydrate(probe: Probe) -> None:
    """hook 注入的 append_system_prompt 在 fork / 逐出水合后逐字节不变。"""
    probe.register(
        HOOK_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text="reply three"),
        Turn.of(text="reply four"),
    )
    _install_hook(probe)
    reload_result = await probe.driver_required.http.reload()
    assert reload_result.get("ok") is True, reload_result

    session = await probe.session(model=HOOK_MODEL)
    await session.chat("alpha")
    await session.chat("beta")

    # 创建时 hook 注入生效：system 段带标记
    first = probe.context(HOOK_MODEL, 0)
    assert HOOK_MARKER in (first.system or ""), first.describe()

    # fork：子会话与源会话共享 [system, alpha, reply one, beta] 前缀（前 3 条
    # 非 system 消息逐项一致——第 4 条开始分叉：源是 beta，子会话是 child-next）
    child = await session.fork("current")
    child_view = probe.history(child)
    assert_fork_of(probe.history(session), child_view, "current")
    await child.chat("child-next")
    source_req = probe.request(HOOK_MODEL, 1)  # 源会话 "beta" 请求
    child_req = probe.request(HOOK_MODEL, 2)  # 子会话 "child-next" 请求
    _assert_prefix_identity(source_req, child_req, shared=3)

    # 逐出水合（resume 重建 agent）：同一会话的下一轮请求仍同前缀
    await _evict(probe, child.session_id)
    await _wait_inactive(probe, child.session_id)
    hydrated = await _hydrate(probe, child.session_id)
    await hydrated.chat("after-hydrate")

    hydrated_req = probe.request(HOOK_MODEL, 3)
    # 水合前的全部消息都是水合后请求的前缀（含 system / tools / 开关）
    _assert_prefix_identity(
        child_req, hydrated_req, shared=len(child_req.context().messages)
    )
    assert HOOK_MARKER in hydrated_req.body["messages"][0]["content"]


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_dynamic_state_and_overrides_survive_rehydrate(probe: Probe) -> None:
    """创建 override 与运行时开关在水合后依旧生效，请求前缀不变。"""
    probe.register(STATE_MODEL, Turn.of(text="reply one"), Turn.of(text="reply two"))
    session = await probe.session(
        model=STATE_MODEL,
        system_prompt="PROBE-BASE-PROMPT",
        append_system_prompt="PROBE-APPENDED",
        tools=["Read", "Glob"],
        max_turns=5,
        effort="low",
        yolo=False,
    )
    await session.chat("alpha")

    # 运行时开关（显式动作）——与创建 override 一起构成"该会话的有效状态"
    await probe.driver_required.http.update_session(session.session_id, thinking=False)
    info = await session.info()
    assert info["thinking"] is False, info
    assert info["yolo"] is False, info
    assert info["reasoning_effort"] == "low", info

    await session.chat("beta")
    before_req = probe.request(STATE_MODEL, 1)
    assert "PROBE-BASE-PROMPT" in before_req.body["messages"][0]["content"]
    assert "PROBE-APPENDED" in before_req.body["messages"][0]["content"]
    assert before_req.body.get("enable_thinking") is False

    # 重启语义：逐出（丢内存态）→ 按需水合（resume 重建 agent）
    await _evict(probe, session.session_id)
    await _wait_inactive(probe, session.session_id)
    hydrated = await _hydrate(probe, session.session_id)

    info = await hydrated.info()
    assert info["thinking"] is False, info
    assert info["yolo"] is False, info
    assert info["reasoning_effort"] == "low", info
    assert sorted(info["tools"]) == ["Glob", "Read"], info
    metadata = probe.history(hydrated).metadata() or {}
    assert metadata["max_turns"] == 5, metadata

    await hydrated.chat("gamma")
    after_req = probe.request(STATE_MODEL, 2)

    # 前缀身份逐项对账：水合前后的请求共享全部旧消息（含 system 段与 tools）
    _assert_prefix_identity(before_req, after_req, shared=3)
