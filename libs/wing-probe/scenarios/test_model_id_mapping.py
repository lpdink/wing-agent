"""``id`` ≠ ``name`` 的映射 —— 发 id，上游收 name，三处出口分列三者。

声明 ``{"id": "flash", "name": "upstream-flash-v2"}`` 时两个词各司其职：``id``
是**引用词**（会话创建 / 切换 / 协议 / metadata 只用它），``name`` 是**发给上游的
调用名**（剧本也按它路由——假 Provider 见到别的名字会 500）。钉的不变量：

- 建会话发的是 id，上游收到的请求体 ``model`` 是 name（"id 不外发"）；
- 展示名与身份分列：``model_id`` = flash、``model`` = 调用名、``model_display_name``
  = 声明值——三处出口（``sync_session`` 的 agent 快照 / ``/api/session/info`` /
  落盘 ``metadata.json``）同源同刻；
- **id 是唯一入口**：拿调用名当 ``model_id`` 发（它不在 id 空间）即 400，不会被
  静默当成 id 的别名。
"""

from __future__ import annotations

import pytest

from wing_probe import DriverHttpError, Probe, Turn

#: 引用词（会话/协议/metadata 只用它）。
MAPPED_ID = "flash"
#: 发给上游的调用名（剧本按它路由）。
MAPPED_NAME = "upstream-flash-v2"
#: 展示名（前端素材）。
MAPPED_DISPLAY = "Upstream Flash V2"

MAPPED_SPEC = {
    "id": MAPPED_ID,
    "name": MAPPED_NAME,
    "display_name": MAPPED_DISPLAY,
}


@pytest.mark.probe_env(models=[MAPPED_SPEC])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_id_and_name_are_distinct_reference_words(probe: Probe) -> None:
    """发 id → 上游收 name；三条出口分列 id / 调用名 / 展示名；调用名不可当 id。"""
    probe.register(MAPPED_NAME, Turn.of(text="reply one"), Turn.of(text="reply two"))
    http = probe.driver_required.http

    session = await probe.session(model=MAPPED_ID)

    # ① 订阅重放：身份三件套 + 展示名同刻下发。
    sync = await session.watch.expect("sync_session", within=5.0)
    agent = sync.data["agent"]
    assert agent["model_id"] == MAPPED_ID, agent
    assert agent["model_name"] == MAPPED_NAME, agent
    assert agent["provider_name"] == "probe", agent
    assert agent["model_display_name"] == MAPPED_DISPLAY, agent

    # ② 上游见到的是调用名（剧本命中即证据：名字错了会 500）。
    result = await session.chat("hello")
    assert result.data["subtype"] == "success", result.data
    logged = probe.request(MAPPED_NAME, 0)
    assert logged.model == MAPPED_NAME, logged.describe()
    assert logged.body["model"] == MAPPED_NAME, logged.body

    # ③ /api/session/info：引用词、调用名、展示名分列三个字段。
    info = await http.get_session_info(session.session_id)
    assert info["model_id"] == MAPPED_ID, info
    assert info["model"] == MAPPED_NAME, info
    assert info["provider_name"] == "probe", info
    assert info["model_display_name"] == MAPPED_DISPLAY, info

    # ④ 落盘三元组（probe 独立解析 metadata.json，不依赖网关自述）。
    metadata = probe.history(session).metadata() or {}
    assert metadata.get("model_id") == MAPPED_ID, metadata
    assert metadata.get("model_name") == MAPPED_NAME, metadata
    assert metadata.get("provider_name") == "probe", metadata

    # ⑤ 续跑仍打到同一个调用名（映射稳定，不是一次性翻译）。
    await session.chat("again")
    assert probe.requests.count(MAPPED_NAME) == 2, probe.requests.summary()
    assert probe.requests.count(MAPPED_ID) == 0, probe.requests.summary()

    # ⑥ id 是唯一入口：调用名不在 id 空间，拿它当 model_id 发即 400。
    with pytest.raises(DriverHttpError) as failure:
        await probe.session(model=MAPPED_NAME)
    assert failure.value.status == 400, failure.value.call.render()
    detail = str(failure.value.call.response)
    assert f"unknown model id '{MAPPED_NAME}'" in detail, detail
