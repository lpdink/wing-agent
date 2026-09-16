"""``Probe`` 门面的纯逻辑自测（tasks 4.7/6.1；不起网关）。

覆盖：逃生舱的理由校验与禁用语义、三个内置不变量的聚合（含 session id 与不变量
名）、失败报告渲染（来源标注 / 转储路径 / 逃生舱理由）、``dump()`` 的产物清单
（时间线全帧 / 原始帧 / HTTP 留档 / 请求留档 / 落盘拷贝 / 网关日志 / 摘要）。

整机路径（自举 env + 真网关）由 ``scenarios/`` 覆盖；这里只压门面自己的逻辑，
因此用"构造的 env + 无 driver"来喂输入——dump 与不变量都不依赖网关活着。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from wing_probe import ProbeEnv
from wing_probe.probe import (
    DUMP_FRAMES,
    DUMP_HTTP,
    DUMP_LOG,
    DUMP_LOG_TAIL,
    DUMP_REQUESTS,
    DUMP_SUMMARY,
    DUMP_TIMELINE,
    Probe,
    ProbeError,
    ProbeInvariantError,
    run_invariants,
)
from wing_probe.history import HistoryView

SESSION_ID = "20260101-000000-abcdef01"


def write_history(session_dir: Path, records: list[dict]) -> None:
    session_dir.mkdir(parents=True, exist_ok=True)
    body = "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records)
    (session_dir / "history.jsonl").write_text(body, encoding="utf-8")


def build_probe(tmp_path: Path) -> Probe:
    """构造（不启动）一个 probe：env 可读路径 / provider 留档，driver 未连接。"""
    env = ProbeEnv(tmp_path / "probe-root")
    probe = Probe(env, driver=None, workspace=tmp_path / "probe-root" / "workspace")
    probe.workspace.mkdir(parents=True, exist_ok=True)
    return probe


# ── 逃生舱 ──────────────────────────────────────────────────


def test_without_invariants_requires_reason(tmp_path: Path) -> None:
    """逃生舱必须说明理由（spec「逃生舱必须说明理由」）。"""
    probe = build_probe(tmp_path)
    assert probe.invariants_enabled is True

    for bad in ("", "   ", "\n"):
        with pytest.raises(ProbeError) as failure:
            probe.without_invariants(bad)
        assert "requires a non-empty reason" in str(failure.value)
    with pytest.raises(ProbeError):
        probe.without_invariants(None)  # ty: ignore[invalid-argument-type]
    assert probe.invariants_enabled is True, "非法调用不得改变状态"

    probe.without_invariants("  检查 rewind 中间态：链此刻故意不自洽  ")
    assert probe.invariants_enabled is False
    assert probe.invariants_reason == "检查 rewind 中间态：链此刻故意不自洽"


def test_invariant_report_carries_source_reason_and_dump(tmp_path: Path) -> None:
    """失败报告标注来源、session id、不变量名、逃生舱理由与转储路径。"""
    probe = build_probe(tmp_path)
    session_dir = probe.env.session_dir(SESSION_ID)
    write_history(
        session_dir,
        [
            {"uuid": "u1", "role": "user", "content": "hi"},
            {
                "uuid": "u2",
                "parent_uuid": "u1",
                "role": "event",
                "type": "text",
                "content": "delta",
            },
        ],
    )
    probe.without_invariants("仅演示报告渲染")
    probe._last_dump = tmp_path / "artifacts"

    problems = probe.check_invariants([HistoryView(session_dir)])
    report = probe.invariant_report(problems)

    assert problems, "构造的坏 history 必须被检出"
    assert "built-in invariants (source: wing_probe fixture teardown)" in report
    assert SESSION_ID in report
    assert "[no_transient_records]" in report
    assert "仅演示报告渲染" in report
    assert f"artifacts: {tmp_path / 'artifacts'}" in report


def test_check_invariants_without_driver_is_empty(tmp_path: Path) -> None:
    """未连接 driver 时没有可检查的 session（teardown 之后调用也安全）。"""
    probe = build_probe(tmp_path)

    assert probe.check_invariants() == []
    assert probe.invariant_report([]).startswith(
        "--- built-in invariants (source: wing_probe fixture teardown) ---"
    )
    with pytest.raises(ProbeError):
        probe.sessions


def test_probe_invariant_error_is_assertion_error() -> None:
    """不变量失败是 ``AssertionError``（pytest 当断言失败报告，而非测试自身出错）。"""
    error = ProbeInvariantError("boom")
    assert isinstance(error, AssertionError)
    assert error.report == "boom"


# ── 不变量聚合 ──────────────────────────────────────────────


def test_run_invariants_aggregates_named_checks(tmp_path: Path) -> None:
    """三条内置不变量逐条运行，问题带不变量名与 session id（构造历史）。"""
    healthy = tmp_path / "healthy"
    write_history(
        healthy / SESSION_ID,
        [
            {"uuid": "u1", "role": "user", "content": "hi"},
            {"uuid": "u2", "parent_uuid": "u1", "role": "assistant", "content": "ok"},
        ],
    )
    assert run_invariants([HistoryView(healthy / SESSION_ID)]) == []

    broken = tmp_path / "broken"
    write_history(
        broken / SESSION_ID,
        [
            {"uuid": "u1", "role": "user", "content": "hi"},
            # 瞬态 delta 落盘
            {
                "uuid": "u2",
                "parent_uuid": "u1",
                "role": "event",
                "type": "reasoning",
                "content": "thinking…",
            },
            # 断链：parent 不存在
            {
                "uuid": "u3",
                "parent_uuid": "missing",
                "role": "assistant",
                "content": "ok",
            },
            # 孤儿 tool 消息
            {
                "uuid": "u4",
                "parent_uuid": "u3",
                "role": "tool",
                "tool_call_id": "call_x",
                "content": "result",
            },
        ],
    )
    problems = run_invariants([HistoryView(broken / SESSION_ID)])

    names = {problem.split("]")[0].lstrip("[") for problem in problems}
    assert names == {"chain_topology", "tool_pairing", "no_transient_records"}, problems
    assert all(SESSION_ID in problem for problem in problems)
    assert all("history.jsonl" in problem for problem in problems)


def test_run_invariants_checks_every_view(tmp_path: Path) -> None:
    """多 session：每个视图都跑一遍，问题各归各的 session id。"""
    views = []
    for session_id in ("sid-a", "sid-b"):
        session_dir = tmp_path / session_id / session_id
        write_history(
            session_dir,
            [
                {"uuid": f"{session_id}-1", "role": "user", "content": "hi"},
                {
                    "uuid": f"{session_id}-2",
                    "parent_uuid": f"{session_id}-1",
                    "role": "event",
                    "type": "text",
                    "content": "delta",
                },
            ],
        )
        views.append(HistoryView(session_dir))

    problems = run_invariants(views)

    assert len(problems) == 2, problems
    assert any("sid-a" in problem for problem in problems)
    assert any("sid-b" in problem for problem in problems)


# ── 现场转储 ────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_dump_writes_artifacts_inventory(tmp_path: Path) -> None:
    """``dump()`` 写出时间线 / 原始帧 / HTTP / 请求 / 日志 / 摘要，返回目录。"""
    probe = build_probe(tmp_path)
    probe.env.provider.requests.record(
        {"model": "probe/x", "messages": [{"role": "user", "content": "hi"}]},
        model="probe/x",
    )
    probe.env.root.mkdir(parents=True, exist_ok=True)
    probe.env.log_path.write_text("--- gateway log line ---\n", encoding="utf-8")

    target = await probe.dump()

    assert target == probe.env.artifacts_path, "默认落到 <env.root>/artifacts"
    assert target.is_dir()
    written = {path.name for path in target.iterdir()}
    assert {
        DUMP_SUMMARY,
        DUMP_TIMELINE,
        DUMP_FRAMES,
        DUMP_HTTP,
        DUMP_REQUESTS,
        DUMP_LOG,
        DUMP_LOG_TAIL,
        "sessions",
    } <= written, sorted(written)

    summary = (target / DUMP_SUMMARY).read_text(encoding="utf-8")
    assert "--- wing-probe artifacts ---" in summary
    assert f"workspace: {probe.workspace}" in summary
    assert "probe/x×1" in summary
    requests = json.loads((target / DUMP_REQUESTS).read_text(encoding="utf-8"))
    assert requests[0]["model"] == "probe/x"
    assert requests[0]["body"]["messages"] == [{"role": "user", "content": "hi"}]
    assert "gateway log line" in (target / DUMP_LOG_TAIL).read_text(encoding="utf-8")
    assert (target / DUMP_TIMELINE).read_text(encoding="utf-8") == ""
    assert (target / DUMP_FRAMES).read_text(encoding="utf-8") == ""
    assert (target / DUMP_HTTP).read_text(encoding="utf-8") == ""


@pytest.mark.asyncio
async def test_dump_accepts_explicit_path_and_records_reason(tmp_path: Path) -> None:
    """``dump(path=...)`` 可指定目录；逃生舱理由写进摘要。"""
    probe = build_probe(tmp_path)
    probe.without_invariants("演练 rewind 半程状态")
    target = tmp_path / "custom-artifacts"

    returned = await probe.dump(target)

    assert returned == target.resolve()
    summary = (target / DUMP_SUMMARY).read_text(encoding="utf-8")
    assert "built-in invariants DISABLED" in summary
    assert "演练 rewind 半程状态" in summary
    assert probe.artifacts_path not in {path for path in target.iterdir()}, (
        "自定义目录不得写进默认 artifacts"
    )


@pytest.mark.asyncio
async def test_dump_copies_session_files(tmp_path: Path) -> None:
    """落盘拷贝：session 的 history.jsonl / metadata.json 进 ``sessions/<id>/``。"""
    probe = build_probe(tmp_path)
    session_dir = probe.env.session_dir(SESSION_ID)
    write_history(
        session_dir,
        [{"uuid": "u1", "role": "user", "content": "hi"}],
    )
    (session_dir / "metadata.json").write_text(
        json.dumps({"workspace": str(probe.workspace)}), encoding="utf-8"
    )

    target = await probe.dump()

    copied = target / "sessions" / SESSION_ID
    assert (copied / "history.jsonl").read_text(encoding="utf-8") == (
        session_dir / "history.jsonl"
    ).read_text(encoding="utf-8")
    assert json.loads((copied / "metadata.json").read_text(encoding="utf-8")) == {
        "workspace": str(probe.workspace)
    }


# ── 文件断言器解析的 workspace（review N5） ─────────────────


def test_files_of_session_id_reads_metadata_workspace(tmp_path: Path) -> None:
    """只给 session id 时，workspace 从落盘 metadata 回读（不落到默认目录）。"""
    probe = build_probe(tmp_path)
    session_dir = probe.env.session_dir(SESSION_ID)
    write_history(session_dir, [{"uuid": "u1", "role": "user", "content": "hi"}])
    other = tmp_path / "elsewhere"
    other.mkdir()
    (session_dir / "metadata.json").write_text(
        json.dumps({"workspace": str(other)}), encoding="utf-8"
    )

    files = probe.files_of(SESSION_ID)

    assert files.root == other.resolve()
    (other / "out.txt").write_text("done\n", encoding="utf-8")
    files.assert_content("out.txt", equals="done\n")


def test_files_of_falls_back_to_default_workspace(tmp_path: Path) -> None:
    """metadata 缺失 / 无 workspace 时回退默认 workspace（而不是指向不存在目录）。"""
    probe = build_probe(tmp_path)

    assert probe.files_of(SESSION_ID).root == probe.workspace

    session_dir = probe.env.session_dir(SESSION_ID)
    session_dir.mkdir(parents=True)
    (session_dir / "metadata.json").write_text(json.dumps({}), encoding="utf-8")
    assert probe.files_of(SESSION_ID).root == probe.workspace
