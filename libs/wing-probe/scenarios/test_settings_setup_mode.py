"""setup mode 的钱场景：坏配置 → 降级启动 → 经 API 就地修好 → 会话真的能跑起来。

这是「首次运行 = 向导，用户永远见不到 YAML」这句话的唯一机器证据，也是整个
设计的地基（总设计 §1.2：配置非法 ⇒ 网关进程直接崩溃退出，最需要设置面板的
那一刻后端不存在）。断言逐条对应 §8：

1. 进程活着且 ``/api/health`` 200（§8.2 降级启动）；
2. ``schema`` / ``get`` / ``status`` 三条读路径可用（§8.3 白名单，§7.4 表）；
3. 受限路径一律 **503 且 ``error == "setup_mode"``**（§8.3 + 增补 P4 的错误码）；
4. ``/ws`` 握手被拒（§8.3 末条：accept 之前 close，客户端看到 403 升级拒绝）；
5. ``status`` 报 ``valid=false`` + **精确 problems**（字段级 + 跨字段两类来源）；
6. ``POST /api/settings/set`` 一份合法文档 → ``setup_mode_exited=true`` +
   六项「进入正常模式」明细（§8.5；与热重载的六项**不是**同一份列表）；
7. **就地**：进程 pid 不变（不偷偷重启）；
8. 守门解除：``/api/session/create`` 200、``/api/settings/status`` ``valid=true``；
9. WS 可连（``connect_driver``）→ 建会话跑一轮成功（假 Provider 收到请求）→
   ``history.jsonl`` 有记录。

**已知上游偏差**（04 Assumption 2）：``get`` / ``status`` 的 ``setup_mode`` 字段在
setup mode 下仍恒 ``False``（``routes/settings.py`` 写死）。本场景因此**不**用该
字段判降级——判据是会话端点的 503 错误码与 WS 拒绝（两者都是可观测事实）。
"""

from __future__ import annotations

import pytest
import yaml

from wing_probe import (
    DriverHttp,
    DriverHttpError,
    GatewayWS,
    Probe,
    Turn,
    WsError,
    ws_url,
)

#: 场景私有 model 名（修复文档里声明它；剧本按 model 名路由）。
SETUP_MODEL = "probe/settings-setup"

#: 故意不可用的配置：
#: - ``providers[0].protocol: grpc`` → 字段级 ``invalid_value``（Literal 校验）；
#: - ``agents[0].model: nope`` → 跨字段 ``unknown_reference``（不在 id 空间）。
#: 保留一条合法的模型声明：目录为空时实现不报「未命中」（problems.py 的注释），
#: 那会把跨字段问题吞掉、少一类断言。
BROKEN_CONFIG = f"""\
# probe: deliberately unusable config (setup mode money scenario)
providers:
  - name: probe
    protocol: grpc
    base_url: http://127.0.0.1:1/v1
    api_key: probe-key
    models: [{SETUP_MODEL}]
agents:
  - name: default
    model: nope
"""

#: 启动时的问题清单（path + kind 精确匹配，两类来源各一条）。
EXPECTED_PROBLEMS = {
    ("providers[0].protocol", "invalid_value"),
    ("agents[0].model", "unknown_reference"),
}

#: 「进入正常模式」六步的明细名字序（04 D3：与 reload_system 的六项不是同一份）。
SETUP_RELOAD_ITEMS = [
    "config.yaml",
    "log level",
    "prompt commands",
    "runtime",
    "background jobs",
    "auth",
]

#: setup mode 下受限的端点（不在守门白名单里）。
RESTRICTED = ("/api/session/list", "/api/models", "/api/tools")


@pytest.mark.probe_env(connect=False, config_text=BROKEN_CONFIG)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_broken_config_repairs_in_place(probe: Probe) -> None:
    """坏配置降级启动 → 503 setup_mode → 修好 → 就地转入正常模式 → 跑通一轮。"""
    # connect=False：setup mode 下 /ws 在 accept 之前被拒，默认连接会直接失败。
    assert probe.driver is None, "connect=False should leave the driver unconnected"

    # 剧本在修复前就注册好（假 Provider 与网关进程无关）。
    probe.register(SETUP_MODEL, Turn.of(text="repaired and running"))

    http = DriverHttp(probe.env.gateway_url, started_at=probe.env.started_at)
    try:
        pid = probe.env.process.pid
        assert probe.env.process.poll() is None, "gateway died on a broken config"

        # ① 降级启动：进程活着、health 通（白名单里的最小事实）。
        health = await http.health()
        assert health["status"] == "ok", health

        # ② 三条读路径可用（修复所需的素材）。
        schema = await http.request("GET", "/api/settings/schema")
        assert (schema["root"]["key"], schema["root"]["path"]) == ("config", "config")
        broken = await http.request("GET", "/api/settings/get")
        assert broken["values"]["providers"][0]["protocol"] == "grpc", broken["values"]
        assert broken["values"]["providers"][0]["api_key"] is None, broken["values"]

        # ③ 受限路径：503 + error="setup_mode"（P4；不是通用的 service_unavailable）。
        for path in RESTRICTED:
            with pytest.raises(DriverHttpError) as failure:
                await http.request("GET", path)
            call = failure.value.call
            assert call.status == 503, call.render()
            assert call.response["error"] == "setup_mode", call.response
        with pytest.raises(DriverHttpError) as create_failure:
            await http.request(
                "POST", "/api/session/create", body={"workspace": str(probe.workspace)}
            )
        assert create_failure.value.status == 503, create_failure.value.call.render()
        refusal = create_failure.value.call.response
        assert refusal["error"] == "setup_mode", refusal
        # detail 是「为什么降级 + 怎么修」的可操作摘要（§8.3 render_setup_detail）。
        assert "providers[0].protocol" in refusal["detail"], refusal
        assert "agents[0].model" in refusal["detail"], refusal

        # ④ status：valid=false + 精确 problems（字段级 + 跨字段）。
        status = await http.request("GET", "/api/settings/status")
        assert status["valid"] is False, status
        assert {
            (p["path"], p["kind"]) for p in status["problems"]
        } == EXPECTED_PROBLEMS, status["problems"]

        # ⑤ WS 在 accept 之前被拒（客户端看到 403 升级拒绝，不是连上再断）。
        with pytest.raises(WsError) as ws_failure:
            await GatewayWS.connect(ws_url(probe.env.gateway_url))
        assert "403" in str(ws_failure.value), str(ws_failure.value)

        # ⑥ 修复：提交一份合法文档（base 省略 = 增补 P5 的 null 路径）。
        before_bytes = probe.env.config_path.read_bytes()
        fixed = {
            "providers": [
                {
                    "name": "probe",
                    "protocol": "openai",
                    "base_url": probe.env.provider.base_url,
                    "api_key": "probe-key",
                    "models": [SETUP_MODEL],
                }
            ],
            "agents": [
                {
                    "name": "default",
                    "model": SETUP_MODEL,
                    "default": True,
                    "system_prompt": probe.env.system_prompt,
                    "tools": ["Bash"],
                    "context_window_tokens": 256_000,
                    "keep_recent_tokens": 50_000,
                }
            ],
        }
        receipt = await http.request(
            "POST", "/api/settings/set", body={"document": fixed}
        )
        assert receipt["ok"] is True, receipt
        assert receipt["problems"] == [], receipt
        assert receipt["setup_mode_exited"] is True, receipt
        assert receipt["restart_required"] == [], receipt
        assert {
            "providers[0].protocol",
            "agents[0].model",
            "providers[0].base_url",
        } <= set(receipt["changed"]), receipt["changed"]
        assert [
            item["name"] for item in receipt["reload"]["results"]
        ] == SETUP_RELOAD_ITEMS, receipt["reload"]
        assert [item["ok"] for item in receipt["reload"]["results"]] == [True] * 6, (
            receipt["reload"]
        )
        # 坏文件被备份（⑤ 步：存在才备份）。
        assert receipt["backup_path"] is not None, receipt
        backup = probe.env.config_path.with_name("config.yaml.bak")
        assert backup.read_bytes() == before_bytes, receipt["backup_path"]

        # ⑦ 就地转入：同一个进程（pid 不变、仍活着）——不是偷偷重启。
        assert probe.env.process.pid == pid, (pid, probe.env.process.pid)
        assert probe.env.process.poll() is None

        # ⑧ 守门解除：同一 HTTP 客户端再打会话端点 → 200；status 转 valid。
        created = await http.request(
            "POST", "/api/session/create", body={"workspace": str(probe.workspace)}
        )
        assert created["session_id"], created
        status = await http.request("GET", "/api/settings/status")
        assert status["valid"] is True, status
        assert status["problems"] == [], status

        # ⑨ 盘上就是我们提交的那份（YAML 可解析、字段一致）。
        written = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
        assert written["providers"][0]["protocol"] == "openai", written["providers"][0]
        assert written["providers"][0]["base_url"] == probe.env.provider.base_url, (
            written["providers"][0]
        )
        assert written["agents"][0]["model"] == SETUP_MODEL, written["agents"][0]

        # ⑩ WS 可连 + 一轮真对话（假 Provider 收到请求）+ 落盘有记录。
        driver = await probe.connect_driver()
        assert driver.client_id, driver
        session = await probe.session(model=SETUP_MODEL)
        result = await session.chat("are you back?")
        assert result.data["subtype"] == "success", result.data
        assert result.data["result"] == "repaired and running", result.data

        request = probe.request(SETUP_MODEL, 0)
        assert request.context().messages[-1].content == "are you back?", (
            request.describe()
        )

        history = probe.history(session)
        assert [message["role"] for message in history.messages()] == [
            "user",
            "assistant",
        ], history.describe()
        history.assert_chain_invariants()
        session.watch.assert_never("error")
    finally:
        await http.close()
