"""非法文档被拒 —— ``ok=false`` + 精确 path + **一个字节都没写** + 会话照常。

保存事务的核心承诺是**全有或全无**（总设计 §7.3）：④ 校验失败时既不能写盘、也不能
备份、更不能动在跑的会话。协议层的取舍是"校验失败走 HTTP 200 + ``ok=false``"
（D16）——请求本身合法，是用户填的内容不合法，所以这里连状态码都要钉住：
一旦有人把它改成 4xx，前端保存路径就会开始报"网络/协议错误"。

覆盖的断言点：

- 一次保存里同时制造**字段级**（``providers[0].protocol="grpc"`` → ``invalid_value``）
  与**跨字段**（``agents[0].model="nope"`` → ``unknown_reference``）两类问题：
  ``problems`` 的 path 集合精确匹配（不是"至少有一条"）、kind 逐条正确、hint 可操作；
- HTTP 200（不是 422/400）；``changed`` / ``restart_required`` 空、``reload`` 为 None、
  ``backup_path`` 为 None（事务没走到 ⑤）；
- **读盘 sha256 与 set 前逐字节一致**；``status`` 仍 ``valid=true``；
- 既有会话照常跑完一轮（假 Provider 收到请求）——拒绝保存 ≠ 弄坏在跑的会话。
"""

from __future__ import annotations

import hashlib

import pytest

from wing_probe import Probe, Turn

#: 场景私有 model 名。
INVALID_MODEL = "probe/settings-invalid"


@pytest.mark.probe_env(models=[INVALID_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_invalid_values_are_rejected_without_touching_the_file(
    probe: Probe,
) -> None:
    """非法值 → ok=false + 精确 problems（两类来源）+ 文件字节未变 + 会话照常。"""
    probe.register(INVALID_MODEL, Turn.of(text="before"), Turn.of(text="after"))
    session = await probe.session(model=INVALID_MODEL)
    http = probe.driver_required.http

    first = await session.chat("before the reject")
    assert first.data["subtype"] == "success", first.data

    current = await http.request("GET", "/api/settings/get")
    document = current["values"]
    document["providers"][0]["protocol"] = "grpc"  # 字段级（Literal）
    document["agents"][0]["model"] = "nope"  # 跨字段（不在 id 空间）

    before_bytes = probe.env.config_path.read_bytes()
    before_digest = hashlib.sha256(before_bytes).hexdigest()

    receipt = await http.request(
        "POST",
        "/api/settings/set",
        body={"base": current["fingerprint"], "document": document},
    )

    # D16：校验失败是业务结果，HTTP 200（不是 4xx）。
    call = probe.last_http_call(path="/api/settings/set")
    assert call is not None and call.status == 200, call.render() if call else "no call"

    assert receipt["ok"] is False, receipt
    assert receipt["fingerprint"] == current["fingerprint"], receipt
    assert receipt["changed"] == [], receipt
    assert receipt["restart_required"] == [], receipt
    assert receipt["reload"] is None, receipt
    assert receipt["backup_path"] is None, receipt
    assert receipt["setup_mode_exited"] is False, receipt

    problems = {problem["path"]: problem for problem in receipt["problems"]}
    assert set(problems) == {"providers[0].protocol", "agents[0].model"}, receipt[
        "problems"
    ]
    assert problems["providers[0].protocol"]["kind"] == "invalid_value", problems
    assert "openai" in problems["providers[0].protocol"]["message"], problems
    assert problems["agents[0].model"]["kind"] == "unknown_reference", problems
    assert "nope" in problems["agents[0].model"]["message"], problems
    assert problems["agents[0].model"]["hint"], problems

    # 全有或全无：文件逐字节未变（⑤ 之前就返回了）。
    assert probe.env.config_path.read_bytes() == before_bytes
    assert (
        hashlib.sha256(probe.env.config_path.read_bytes()).hexdigest() == before_digest
    )

    # status 仍说磁盘上的配置是好的（被拒的是"提议的文档"，不是现有文件）。
    status = await http.request("GET", "/api/settings/status")
    assert status["valid"] is True, status
    assert status["problems"] == [], status

    # 既有会话照常收尾（拒绝保存不干扰在跑的会话）。
    second = await session.chat("after the reject")
    assert second.data["subtype"] == "success", second.data
    follow_up = probe.request(INVALID_MODEL, 1)
    assert follow_up.context().messages[-1].content == "after the reject", (
        follow_up.describe()
    )
    session.watch.assert_never("error")
