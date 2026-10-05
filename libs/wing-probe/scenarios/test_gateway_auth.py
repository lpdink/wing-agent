"""网关鉴权（opt-in API key）场景：401 / 403 / WS 拒绝与 tool_runtime 的边界。

`gateway.auth.enabled=true` 是**默认关闭**的能力（probe 的其它场景都不打该标记），
它的三条路径今天只有单测（`libs/core/tests/`）覆盖，重构网关中间件时容易悄悄
放宽：

1. **无 key / 坏 key → 401**：`AuthMiddleware` 拦在 handler 之前，`/api/health`
   是唯一免鉴权路径（"网关活着、只是拒绝"的成对证据）；
2. **WS 握手在 accept 之前拒绝**：ASGI 的 close-before-accept 对客户端表现为
   HTTP 403 的升级拒绝（close code / reason 不可见）——无 key 与
   "tool_runtime 不声明 client_id"两条路径都是这一形态；
3. **RBAC**：`tool_runtime` 只被允许 `/api/tools/register`（allowlist 之外一律
   403，不是 401——key 有效但角色不允许）；它也**不能**投递用户消息（帧级 error）
   与**不接收事件**（见"断言强度"一节）。

覆盖的断言点：

- ``test_keyless_and_bad_key_are_rejected_with_401``：health 200 / list 401 /
  create 401 / 坏 key 401 / admin key 200（成对出现）、无 key 的 WS 握手被拒
  （403 升级拒绝，admin key 同 URL 成功）；
- ``test_admin_key_drives_a_normal_session``：admin key 下建会话 + 一轮对话正常
  收尾（有 key 的"可用"半边：请求真的到达假 Provider）；
- ``test_tool_runtime_role_is_scoped_to_tool_registration``：注册工具 200（allowlist
  内）、`/api/session/list` 与 `/api/session/subscribe` 403、用户消息被拒（帧级
  error 帧）、不声明 client_id 的 WS 被拒、一轮完整会话期间零事件帧。

**断言强度（如实声明）**：产品对 tool_runtime 的"不接收事件"（`receives_events=
False`）只在全局广播（`scope="global"`）上产生可观测差异，而当前代码库没有任何
global 发射点——直接观测不到。本场景用三条可观测旁证替代：(a) 它无法订阅任何会话
（403）；(b) 一轮完整会话期间它的帧数不变；(c) 网关日志的 attach 行含
`receives_events=False`（文本耦合，与 02a 对产品文案的逐字断言同口径）。
"""

from __future__ import annotations

import asyncio
import json
from typing import Any

import pytest

from wing_probe import (
    DriverHttp,
    DriverHttpError,
    GatewayWS,
    Probe,
    Turn,
    WsError,
    ws_url,
)
from wing_probe.driver import Delivery

#: 环境旋钮：auth 打开 + 两把角色不同的 key；driver 用 admin key 连接。
ADMIN_KEY = "probe-admin-key"
TOOL_RUNTIME_KEY = "probe-tool-key"
WRONG_KEY = "probe-wrong-key"
AUTH_KEYS = [
    {"key": ADMIN_KEY},
    {"key": TOOL_RUNTIME_KEY, "role": "tool_runtime"},
]

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
ADMIN_MODEL = "probe/auth-admin"

#: tool host 声明的 client_id（命名空间 = 注册工具的引用前缀）。
TOOL_HOST_ID = "probe-toolhost"

#: 注册的远程工具规格（schema 与内置工具同构）。
TOOL_SPEC = {
    "name": "ProbeEcho",
    "description": "probe remote echo tool",
    "params": [
        {
            "name": "text",
            "type": "string",
            "description": "text to echo",
        }
    ],
}

#: 帧等待预算（本地 WS，实测亚秒级；放宽只为 CI 抖动）。
FRAME_WITHIN = 5.0


def _auth_env() -> dict:
    return {"enabled": True, "keys": AUTH_KEYS}


def _http_client(probe: Probe, api_key: str | None = None) -> DriverHttp:
    """独立的 HTTP 客户端（不带头 / 显式带 key），与 fixture driver 的留档分离。"""
    return DriverHttp(probe.env.gateway_url, api_key, started_at=probe.env.started_at)


def _error_body(failure: DriverHttpError) -> dict:
    response = failure.call.response
    assert isinstance(response, dict), failure.call.render()
    return response


@pytest.mark.probe_env(auth=_auth_env(), api_key=ADMIN_KEY)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_keyless_and_bad_key_are_rejected_with_401(probe: Probe) -> None:
    """无 key / 坏 key 一律 401；health 免鉴权、admin key 放行（成对证据）。"""
    keyless = _http_client(probe)
    try:
        # 免鉴权路径：网关活着（否则下面的 401 可能只是"连不上"的伪装）。
        health = await keyless.health()
        assert health, health
        health_call = keyless.last_call(path="/api/health")
        assert health_call is not None and health_call.status == 200

        # 读方法：拦在 handler 之前。
        with pytest.raises(DriverHttpError) as read_failure:
            await keyless.list_sessions()
        assert read_failure.value.status == 401, read_failure.value.call.render()
        assert _error_body(read_failure.value) == {
            "error": "unauthorized",
            "detail": "Invalid or missing API key",
        }, read_failure.value.call.response

        # 写方法：同样被拦（不是"只保护查询"）。
        with pytest.raises(DriverHttpError) as create_failure:
            await keyless.create_session(workspace=str(probe.workspace))
        assert create_failure.value.status == 401, create_failure.value.call.render()
    finally:
        await keyless.close()

    # 坏 key 与"没有 key"同类（verify 失败 → 401，不泄露"key 存在但错"）。
    bad_key = _http_client(probe, WRONG_KEY)
    try:
        with pytest.raises(DriverHttpError) as failure:
            await bad_key.list_sessions()
        assert failure.value.status == 401, failure.value.call.render()
    finally:
        await bad_key.close()

    # 成对：同一端点上 admin key 是 200（401 不是网关整体不可用）。
    allowed = await probe.driver_required.http.list_sessions()
    assert allowed == {"sessions": []}, allowed

    # WS：未鉴权连接在 **accept 之前**被拒——ASGI 的 close-before-accept 对客户端
    # 表现为 HTTP 403 的升级拒绝（拿不到 close code / reason，也拿不到
    # ConnectResponse）。同一条 URL 用 admin key 必须成功（成对）。
    with pytest.raises(WsError) as ws_failure:
        await GatewayWS.connect(ws_url(probe.env.gateway_url))
    assert "403" in str(ws_failure.value), str(ws_failure.value)

    accepted = await GatewayWS.connect(ws_url(probe.env.gateway_url), api_key=ADMIN_KEY)
    try:
        assert accepted.client_id, accepted.client_id
    finally:
        await accepted.close()


@pytest.mark.probe_env(auth=_auth_env(), api_key=ADMIN_KEY)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_admin_key_drives_a_normal_session(probe: Probe) -> None:
    """admin key 下会话全链路可用（有 key 的"可用"半边）。"""
    probe.register(ADMIN_MODEL, Turn.of(text="authorized reply"))
    session = await probe.session(model=ADMIN_MODEL)

    result = await session.chat("hello with a key")

    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == "authorized reply", result.data
    session.watch.assert_never("error")
    # 请求真的到达假 Provider（不是"会话对象建起来了但链路没通"）。
    assert probe.request(ADMIN_MODEL, 0).context().messages[-1].content == (
        "hello with a key"
    )
    # 会话的运行时信息也可查（鉴权不改变既有端点行为）。
    info = await session.info()
    assert info["status"] == "idle", info


@pytest.mark.probe_env(auth=_auth_env(), api_key=ADMIN_KEY)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_tool_runtime_role_is_scoped_to_tool_registration(probe: Probe) -> None:
    """tool_runtime：只允许注册工具；不能订阅/投递消息，也不接收事件。"""
    probe.register(ADMIN_MODEL, Turn.of(text="admin reply"))
    admin_session = await probe.session(model=ADMIN_MODEL)

    # tool host：WS 声明 client_id（query param）+ tool_runtime key（header）。
    frames: list[tuple[str | None, str, dict]] = []
    frame_arrived = asyncio.Event()

    def on_event(
        session_id: str | None,
        event_type: str,
        data: dict[str, Any],
        delivery: Delivery,
    ) -> None:
        frames.append((session_id, event_type, data))
        frame_arrived.set()

    tool_ws = await GatewayWS.connect(
        f"{ws_url(probe.env.gateway_url)}?client_id={TOOL_HOST_ID}",
        api_key=TOOL_RUNTIME_KEY,
        handler=on_event,
    )
    try:
        assert tool_ws.client_id == TOOL_HOST_ID, tool_ws.client_id
        # 握手帧不计入读任务的计数：此刻零事件帧。
        assert tool_ws.frames_received == 0

        # ① allowlist 内：能注册工具（注册要求该 client_id 有活跃 WS）。
        tool_http = _http_client(probe, TOOL_RUNTIME_KEY)
        try:
            registered = await tool_http.request(
                "POST",
                "/api/tools/register",
                body={"tools": [TOOL_SPEC]},
                headers={"X-Client-Id": TOOL_HOST_ID},
            )
            assert registered == {
                "ok": True,
                "registered": [f"{TOOL_HOST_ID}.ProbeEcho"],
            }, registered
            # 注册真的进了全局注册表（LLM 可见名 = 声明的 llm_name / name）。
            listed = await probe.driver_required.http.list_tools()
            names = {tool["ref"] for tool in listed["tools"]}
            assert f"{TOOL_HOST_ID}.ProbeEcho" in names, sorted(names)

            # ② allowlist 外：403（不是 401——key 有效、角色不允许）。
            with pytest.raises(DriverHttpError) as list_failure:
                await tool_http.list_sessions()
            assert list_failure.value.status == 403, list_failure.value.call.render()
            assert _error_body(list_failure.value)["error"] == "forbidden"

            with pytest.raises(DriverHttpError) as subscribe_failure:
                await tool_http.subscribe(admin_session.session_id, TOOL_HOST_ID)
            assert subscribe_failure.value.status == 403, (
                subscribe_failure.value.call.render()
            )
        finally:
            await tool_http.close()

        # ③ 投递用户消息被拒：帧级 error（不是静默丢弃、不是断连）。
        await tool_ws.send_request(admin_session.session_id, "tool host cannot drive")
        await asyncio.wait_for(frame_arrived.wait(), FRAME_WITHIN)
        assert len(frames) == 1, frames
        session_id, event_type, data = frames[0]
        assert event_type == "error", frames
        assert "tool_runtime role cannot send user messages" in json.dumps(
            data, ensure_ascii=False
        ), data
        assert session_id is None, frames

        # ④ 一轮完整会话期间：tool host 零事件帧（它订阅不了任何会话）。
        result = await admin_session.chat("drive the admin session")
        assert result.data["subtype"] == "success", result.data
        assert tool_ws.frames_received == 1, [frame[1] for frame in frames]

        # ⑤ 网关日志记下"attach 但不收事件"（见文件头"断言强度"：这是本机制在
        #    当前代码库里唯一可观测的表征）。
        log_text = probe.env.log_path.read_text(encoding="utf-8", errors="replace")
        attach_lines = [
            line for line in log_text.splitlines() if "attached tool host" in line
        ]
        assert any(
            TOOL_HOST_ID in line and "receives_events=False" in line
            for line in attach_lines
        ), attach_lines or log_text[-2000:]
    finally:
        await tool_ws.close()

    # ⑥ tool_runtime 不声明 client_id → 握手被拒（同行（4009）在 wire 上与 4001
    #    无法区分：两者都是 close-before-accept → 403；这里断言"必须声明"这条
    #    路径同样被拒，而不是静默地当纯前端放行）。
    with pytest.raises(WsError) as no_decl_failure:
        await GatewayWS.connect(ws_url(probe.env.gateway_url), api_key=TOOL_RUNTIME_KEY)
    assert "403" in str(no_decl_failure.value), str(no_decl_failure.value)
