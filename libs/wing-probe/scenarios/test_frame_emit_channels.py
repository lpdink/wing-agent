"""发射时刻账本的通道标记（跨进程配对的前提）。

覆盖点：``FrameEmit.kind`` 让「假 Provider 的帧」与「客户端事件」可按下标配对——
``text`` 长度 L、``chunk=1`` 的流式轮次，``kind == "content"`` 的帧数必须等于客户端
收到的 ``text`` 事件数（都等于 L）；其余帧是 ``meta`` / ``thinking`` / ``tool_call``，
不与 ``text`` 事件混。账本经真网关跑，因此这条不变量在跨进程场景下也成立。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间不共享）。
TEXT_MODEL = "probe/frame-emits-text"
THINKING_MODEL = "probe/frame-emits-thinking"

TEXT = "abcdefgh"


def frame_emits_of(probe: Probe, model: str) -> list:
    """该 model 唯一一次请求的逐帧记录（按 ``frame_index`` 升序）。"""
    request_index = probe.request(model).index
    emits = [e for e in probe.provider.frame_emits if e.request_index == request_index]
    # 冻结视图（只有时刻）与账本条数必须一致：两条访问路径同一个账本。
    assert len(probe.provider.emits_for(request_index)) == len(emits)
    assert [e.frame_index for e in emits] == list(range(len(emits)))
    return emits


@pytest.mark.probe_env(models=[TEXT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_content_frames_match_text_events(probe: Probe) -> None:
    """``kind == "content"`` 的帧数 == 客户端 ``text`` 事件数 == 文本长度。

    WHEN 剧本吐 ``chunk=1`` 的 8 字符正文
    THEN 客户端收到 8 条 ``text`` 事件、账本里 8 条纹 ``content`` 帧（外加 3 条纹 ``meta``
    帧），两者按 ``frame_index`` 一一对应。
    """
    probe.register(TEXT_MODEL, Turn.of(text=TEXT, chunk=1))
    session = await probe.session(model=TEXT_MODEL)

    result = await session.chat("hi")

    assert result.data["subtype"] == "success", result.data
    assert result.data["result"] == TEXT, result.data

    text_events = session.watch.events("text")
    assert "".join(event.data["content"] for event in text_events) == TEXT
    assert len(text_events) == len(TEXT)

    emits = frame_emits_of(probe, TEXT_MODEL)
    content = [emit for emit in emits if emit.kind == "content"]
    assert len(content) == len(text_events) == len(TEXT)
    assert [emit.kind for emit in emits if emit.kind != "content"] == ["meta"] * 3
    # role 帧在最前、finish / usage 帧在最后：按 kind 筛过之后下标可直接配对。
    assert emits[0].kind == "meta"
    assert [emit.kind for emit in emits[-2:]] == ["meta", "meta"]
    session.watch.assert_never("error")


@pytest.mark.probe_env(models=[THINKING_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_thinking_frames_match_reasoning_events(probe: Probe) -> None:
    """``kind == "thinking"`` 的帧数 == 客户端 ``reasoning`` 事件数；正文帧单独计数。

    WHEN 剧本先吐 4 字符推理再吐 3 字符正文（``chunk=1``）
    THEN 账本 4 条 ``thinking`` + 3 条 ``content``，分别对上客户端的 ``reasoning`` /
    ``text`` 事件数——两类通道互不串。
    """
    probe.register(THINKING_MODEL, Turn.of(thinking="思考一下", text="abc", chunk=1))
    session = await probe.session(model=THINKING_MODEL)

    result = await session.chat("hi")

    assert result.data["subtype"] == "success", result.data
    reasoning_events = session.watch.events("reasoning")
    text_events = session.watch.events("text")
    assert "".join(event.data["content"] for event in reasoning_events) == "思考一下"
    assert "".join(event.data["content"] for event in text_events) == "abc"

    kinds = [emit.kind for emit in frame_emits_of(probe, THINKING_MODEL)]
    assert kinds.count("thinking") == len(reasoning_events) == 4
    assert kinds.count("content") == len(text_events) == 3
    assert kinds.count("meta") == 3
