"""restart 类变更：回执如实说"要重启"，网关**不做假热更**。

``gateway.port`` 的生效域是 ``restart``（设计 §10 全表）：保存事务会把它写进文件、
在回执的 ``restart_required`` 里点名，但**不会**去动正在监听的那条 socket——
热更端口意味着断开所有客户端（TUI 挂在 WS 上），这不是"保存"该有的副作用。

覆盖的断言点：

- 回执：``changed == ["gateway.port"]``、``restart_required == ["gateway.port"]``、
  ``reload`` 照常跑（六项，配置本身已应用——只是端口要等重启）；
- **旧端口仍然健康**：网关进程 pid 不变、``/api/health`` 照常（没有悄悄重启/换绑）；
- **新端口没人监听**（不作假热更：两处都不能"顺手"绑上去）；
- 盘上文件已写新端口（保存是真的发生过的，只是生效时机是重启）。
"""

from __future__ import annotations

import socket
from collections.abc import Mapping

import pytest
import yaml

from wing_probe import DriverHttp, DriverHttpError, Probe, reserve_port

#: 热重载六项（R3：`log level` 追加在末尾）。
RELOAD_ITEMS = [
    "config.yaml",
    "hooks",
    "prompt commands",
    "provider",
    "skills & rules",
    "log level",
]


def _listening(port: int, *, host: str = "127.0.0.1", timeout: float = 1.0) -> bool:
    """该端口当前是否有人监听（连接成功即 True；拒绝/超时 = False）。"""
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def _free_port() -> int:
    """OS 分配一个当前未监听的端口（多抽几次，避开并行场景抢同一个端口的窗口）。

    ``reserve_port()`` 只是 bind(0) 后立即释放——到断言时刻之间理论上可能被别处
    占走；这里先确认"此刻确实没人监听"，把假红窗口压到最小。
    """
    for _ in range(5):
        port = reserve_port()
        if not _listening(port):
            return port
    raise AssertionError("could not draw a free port for the restart-required check")


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_port_change_needs_a_restart_and_does_not_rebind(probe: Probe) -> None:
    """改 gateway.port → restart_required 点名；旧端口照常、新端口无人监听。"""
    http = probe.driver_required.http
    # ty：`process` / `port` 都是可空属性（未启动的 env）——先收窄成非空，后续断言直读。
    process = probe.env.process
    assert process is not None, "probe env must have spawned a gateway process"
    old_port = probe.env.port
    assert old_port is not None, "probe env must know its gateway port"
    pid = process.pid

    current = await http.request("GET", "/api/settings/get")
    document = current["values"]
    new_port = _free_port()
    assert new_port != old_port, new_port
    document["gateway"]["port"] = new_port

    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["changed"] == ["gateway.port"], receipt["changed"]
    assert receipt["restart_required"] == ["gateway.port"], receipt
    # 配置本身是应用的（reload 照常六项），要重启的只是监听端口。
    assert receipt["reload"]["ok"] is True, receipt["reload"]
    assert [item["name"] for item in receipt["reload"]["results"]] == RELOAD_ITEMS, (
        receipt["reload"]
    )

    # 不做假热更：同一个进程、旧端口照常服务。
    assert process.poll() is None
    assert process.pid == pid, (pid, process.pid)
    assert _listening(old_port), f"gateway stopped serving the old port {old_port}"
    health = await http.health()
    assert health["status"] == "ok", health

    # 新端口没人监听（保存不会顺手把第二个监听器绑上去）。
    assert not _listening(new_port), (
        f"gateway silently bound the new port {new_port}: restart-required fields "
        "must not be hot-applied"
    )

    # 保存是真的：盘上文件里是新端口。
    written = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    assert written["gateway"]["port"] == new_port, written["gateway"]


#: 一个新加的跨源来源（loopback 之外；默认不放通）。
CORS_ORIGIN = "https://ui.probe.example"


async def _preflight_headers(
    http: DriverHttp, headers: dict[str, str]
) -> Mapping[str, str]:
    """一次 CORS 预检的响应头。

    未放通时 ``CORSMiddleware`` **就地**回 ``400 "Disallowed CORS origin"``（无
    ACAO；不落路由），放通时 200 + 回声——两条路径都要拿到响应头，所以捕获
    ``DriverHttpError``。
    """
    try:
        call, _ = await http.request_raw(
            "OPTIONS", "/api/workspace/list", headers=headers
        )
    except DriverHttpError as exc:
        return exc.call.response_headers
    return call.response_headers


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_cors_origins_change_names_the_restart_path(probe: Probe) -> None:
    """改 ``gateway.cors.origins`` → 回执点名元素路径；运行中的网关不做假热更。

    CORS 中间件在 app 装配（进程启动）时读配置——保存事务写文件 + 在
    ``restart_required`` 里点名，运行中的网关**不放通新来源**。元素路径
    （``origins[0]``）也要被点名：标量列表元素继承列表声明的生效域，否则这条
    信号会被静默吞掉（面板的 Ctrl+R 提示因此永远不出现）。

    「重启后真的生效」由单测覆盖（``test_gateway_cors.py`` 用配置文件构造 app，
    预检放通新来源）；probe 侧不重启——``ProbeEnv`` 重启会按旋钮重新生成配置，
    文件里的保存结果不跨重启保留，这是 env 的已知语义。
    """
    http = probe.driver_required.http
    current = await http.request("GET", "/api/settings/get")
    document = current["values"]
    document.setdefault("gateway", {})["cors"] = {"origins": [CORS_ORIGIN]}

    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )
    assert receipt["ok"] is True, receipt
    assert receipt["changed"] == ["gateway.cors.origins[0]"], receipt["changed"]
    assert receipt["restart_required"] == ["gateway.cors.origins[0]"], receipt
    assert receipt["reload"]["ok"] is True, receipt["reload"]

    # 假热更的负向证据：运行中的网关仍不放通新来源（预检就地吃 400，无 ACAO）。
    preflight = {"Origin": CORS_ORIGIN, "Access-Control-Request-Method": "GET"}
    headers = await _preflight_headers(http, preflight)
    assert headers.get("access-control-allow-origin") is None, dict(headers)

    # 保存是真的：盘上文件里已经有新来源（生效时机才是重启）。
    written = yaml.safe_load(probe.env.config_path.read_text(encoding="utf-8"))
    assert written["gateway"]["cors"]["origins"] == [CORS_ORIGIN], written["gateway"]
