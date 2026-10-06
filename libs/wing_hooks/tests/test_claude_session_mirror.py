"""claude_session_mirror hook 的单元测试。

覆盖任务书要求的四类断言：**事件 → 转录行映射**（user / assistant（含
tool_use / thinking）/ tool_result）、**文件路径与目录编码生成**、**幂等订阅**
（重复 install 不产生第二个订阅者、不写重复行），外加 cwd 来源链与挂起补写、
异常隔离、标题行（custom-title / last-prompt）。

断言口径对齐 CloudCLI 消费端（只读参考
``claudecodeui/server/modules/providers/list/claude/claude-sessions.provider.ts``
与 ``claude-session-synchronizer.provider.ts``）：行按 ``sessionId`` 过滤、
首行需带 ``sessionId`` + ``cwd``、顺序即 ``message.content`` 块序。
"""

from __future__ import annotations

import json
import re
from collections.abc import Iterator
from pathlib import Path

import pytest

from wing.event import (
    AgentInfo,
    AssistantTurnEvent,
    DoneEvent,
    SessionInitEvent,
    SessionStateChangedEvent,
    SyncSessionEvent,
    TextEvent,
    ToolResultTurnEvent,
    UserMessageAcceptedEvent,
)
from wing.event_bus import EventBus, event_bus
from wing.hooks import HookRegistry, hooks
from wing.schema import Message, ToolCall

from wing_hooks import claude_session_mirror as mirror_mod

#: 契合 SESSION_ID_PATTERN 的 wing 原生 session id（文件名的直接来源）。
SESSION_ID = "20260101-120000-abcdef12"

TIMESTAMP_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$")


@pytest.fixture(scope="session", autouse=True)
def _detach_default_mirror():
    """摘掉模块装载时挂在**全局** event_bus 上的默认投影器。

    测试一律用独立的 EventBus 实例（见 ``mirror`` fixture），但模块导入本身会
    在全局 bus 上装一个默认实例（生产行为：hook 文件被 load_hooks exec 即生效）。
    留着它意味着"任何在全局总线 emit 的测试"都会往默认根（``~/.claude``）写
    文件——测试期摘掉，默认安装本身由
    ``test_install_subscribes_global_event_bus`` 显式断言。
    """
    mirror_mod.uninstall()
    yield
    mirror_mod.uninstall()


@pytest.fixture
def mirror(
    tmp_path: Path,
) -> Iterator[tuple["mirror_mod.ClaudeSessionMirror", EventBus, Path]]:
    """装好投影器的临时环境：独立 EventBus + 临时 claude home。"""
    bus = EventBus()
    home = tmp_path / "claude"
    instance = mirror_mod.install(bus=bus, claude_home=home)
    assert instance is not None
    yield instance, bus, home
    mirror_mod.uninstall(bus)


@pytest.fixture
def project(tmp_path: Path) -> str:
    """会话工作目录（pytest tmp 已是绝对路径，resolve 后逐字节一致）。"""
    path = tmp_path / "my project.v2"
    path.mkdir()
    return str(path.resolve())


def _project_file(home: Path, project: str, session_id: str = SESSION_ID) -> Path:
    return home / "projects" / mirror_mod.encode_cwd(project) / f"{session_id}.jsonl"


def _read_rows(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _assistant_event(
    text: str = "answer", model: str = "probe/mirror"
) -> AssistantTurnEvent:
    message = Message(
        role="assistant",
        content=text,
        reasoning_content="thinking hard",
        tool_calls=[ToolCall(id="call_1", name="Bash", arguments={"command": "ls"})],
    )
    return AssistantTurnEvent.from_message(message, model, SESSION_ID)


def _tool_result_event(success: bool = True) -> ToolResultTurnEvent:
    return ToolResultTurnEvent.from_execution(
        ToolCall(id="call_1", name="Bash", arguments={"command": "ls"}),
        "file-a\nfile-b",
        success,
        SESSION_ID,
    )


# ============================================================
# 事件 → 转录行映射
# ============================================================


def test_turn_maps_to_claude_transcript_rows(mirror, project):
    """一轮对话 → user / assistant（含 thinking + tool_use）/ tool_result 行。"""
    instance, bus, home = mirror
    instance.note_session(SESSION_ID, project)
    assert instance.flush()

    bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hello"))
    bus.emit(_assistant_event())
    bus.emit(_tool_result_event())
    assert instance.flush()

    rows = _read_rows(_project_file(home, project))
    assert [row["type"] for row in rows] == ["user", "last-prompt", "assistant", "user"]

    user, last_prompt, assistant, tool_result = rows

    # 发现契约：首行是合法 JSON 且带 sessionId + cwd
    assert user["sessionId"] == SESSION_ID
    assert user["cwd"] == project
    assert TIMESTAMP_RE.match(user["timestamp"]), user["timestamp"]
    assert user["parentUuid"] is None
    assert user["isMeta"] is False
    assert user["message"] == {
        "role": "user",
        "content": [{"type": "text", "text": "hello"}],
    }

    # 标题来源之一：最近一条用户提示（leafUuid 指向该 user 行）
    assert last_prompt == {
        "type": "last-prompt",
        "lastPrompt": "hello",
        "leafUuid": user["uuid"],
        "sessionId": SESSION_ID,
    }

    assert assistant["parentUuid"] == user["uuid"]
    assert assistant["message"]["role"] == "assistant"
    assert assistant["message"]["model"] == "probe/mirror"
    assert assistant["message"]["content"] == [
        {"type": "thinking", "thinking": "thinking hard"},
        {"type": "text", "text": "answer"},
        {
            "type": "tool_use",
            "id": "call_1",
            "name": "Bash",
            "input": {"command": "ls"},
        },
    ]

    assert tool_result["parentUuid"] == assistant["uuid"]
    assert tool_result["message"] == {
        "role": "user",
        "content": [
            {
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": "file-a\nfile-b",
                "is_error": False,
            }
        ],
    }

    # 链是线性的（消费端的 resolveResumePath / chainFrom 依赖它）
    chained = [row for row in rows if "uuid" in row]
    for previous, current in zip(chained, chained[1:], strict=False):
        assert current["parentUuid"] == previous["uuid"]


def test_tool_result_failure_sets_is_error(mirror, project):
    """失败的 tool result → ``is_error: true``。"""
    instance, bus, home = mirror
    instance.note_session(SESSION_ID, project)
    bus.emit(_tool_result_event(success=False))
    assert instance.flush()

    rows = _read_rows(_project_file(home, project))
    assert rows[0]["message"]["content"][0]["is_error"] is True


def test_assistant_message_without_blocks_is_not_written(mirror, project):
    """空回复（无 thinking / text / tool_use）不产行——不留噪声。"""
    instance, bus, home = mirror
    instance.note_session(SESSION_ID, project)
    bus.emit(
        AssistantTurnEvent.from_message(Message(role="assistant"), "m", SESSION_ID)
    )
    assert instance.flush()

    assert not _project_file(home, project).exists()


def test_unknown_events_are_ignored(mirror, project):
    """与投影无关的事件（流式 delta / turn 结束）不产行。"""
    instance, bus, home = mirror
    instance.note_session(SESSION_ID, project)
    bus.emit(TextEvent(session_id=SESSION_ID, content="partial"))
    bus.emit(DoneEvent(session_id=SESSION_ID))
    assert instance.flush()

    assert not _project_file(home, project).exists()


def test_title_rows_track_session_rename(mirror, project):
    """``session_state_changed.title`` → custom-title 行（去重、保留最后一次）。"""
    instance, bus, home = mirror
    instance.note_session(SESSION_ID, project)
    for title in ("first title", "first title", "renamed"):
        bus.emit(SessionStateChangedEvent(session_id=SESSION_ID, title=title))
    assert instance.flush()

    rows = _read_rows(_project_file(home, project))
    assert rows == [
        {"type": "custom-title", "customTitle": "first title", "sessionId": SESSION_ID},
        {"type": "custom-title", "customTitle": "renamed", "sessionId": SESSION_ID},
    ]


# ============================================================
# 路径与目录编码
# ============================================================


def test_cwd_encoding_follows_claude_projects_convention():
    """非字母数字一律替换成 ``-``（Claude Code 的 projects/<encoded-cwd> 约定）。"""
    assert (
        mirror_mod.encode_cwd("/Users/me/ws/my project.v2")
        == "-Users-me-ws-my-project-v2"
    )
    assert mirror_mod.encode_cwd("/tmp/a_b-c") == "-tmp-a-b-c"


def test_file_path_and_name(mirror, project):
    """落点 = <claude home>/projects/<编码 cwd>/<wing session id>.jsonl。"""
    instance, bus, home = mirror
    instance.note_session(SESSION_ID, project)
    bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))
    assert instance.flush()

    expected = (
        home / "projects" / mirror_mod.encode_cwd(project) / f"{SESSION_ID}.jsonl"
    )
    assert expected.is_file()
    # 同一会话的行全在同一个文件；不同 cwd 的会话进各自目录
    assert [p.name for p in home.glob("projects/*/*.jsonl")] == [f"{SESSION_ID}.jsonl"]


# ============================================================
# cwd 来源链与挂起补写
# ============================================================


def test_cwd_from_session_init_event(mirror, project):
    """没有 before_session_start 时，session_init 事件的 cwd 决定落点。"""
    instance, bus, home = mirror
    bus.emit(SessionInitEvent(session_id=SESSION_ID, cwd=project, model="m"))
    bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))
    assert instance.flush()

    rows = _read_rows(_project_file(home, project))
    assert rows[0]["cwd"] == project


def test_cwd_from_sync_session_workspace(mirror, project):
    """sync_session 的 agent.workspace 是同一时刻的另一份 cwd 来源。"""
    instance, bus, home = mirror
    bus.emit(
        SyncSessionEvent(
            session_id=SESSION_ID,
            status="idle",
            agent=AgentInfo(model_name="m", workspace=project),
        )
    )
    bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))
    assert instance.flush()

    assert _project_file(home, project).is_file()


def test_cwd_from_session_start_hook(monkeypatch, tmp_path):
    """before_session_start 的会话登记（workspace → cwd）生效。"""
    bus = EventBus()
    home = tmp_path / "claude"
    project = tmp_path / "hook project"
    project.mkdir()
    instance = mirror_mod.install(bus=bus, claude_home=home)
    assert instance is not None
    try:
        session = type(
            "FakeSession",
            (),
            {"session_id": SESSION_ID, "session_workspace": str(project)},
        )()
        mirror_mod.claude_mirror_session_start(session)
        bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))
        assert instance.flush()
        assert _project_file(home, str(project.resolve())).is_file()
    finally:
        mirror_mod.uninstall(bus)


def test_session_start_handler_registers_into_global_registry():
    """显式注册到**全局** registry 生效（自包含，不依赖 import 期的现场状态）。

    模块装载（``load_hooks`` exec）时的 import 期注册由
    ``test_module_loads_the_way_load_hooks_does`` 断言；这里只验证「全局注册表
    这条路径本身」——core 的 fixture 会 ``hooks.clear()``，同 session 组合跑时
    "import 期注册仍在场"不是可靠前提（见 conftest 的隔离说明）。
    """
    handler = mirror_mod.claude_mirror_session_start
    hooks.off("before_session_start", handler)
    try:
        mirror_mod.register_claude_session_mirror(hooks)
        assert handler in hooks.handlers("before_session_start")
    finally:
        hooks.off("before_session_start", handler)


def test_module_loads_the_way_load_hooks_does(monkeypatch, tmp_path):
    """按 ``load_hooks`` 的方式装载（``exec_module``、**不注册 sys.modules**）必须成功。

    回归护栏：CPython 3.12 的 dataclasses 在 KW_ONLY 探测里做
    ``sys.modules.get(cls.__module__).__dict__``——钩子文件里用 ``@dataclass``
    会导致整份文件装载失败（"NoneType" object has no attribute '__dict__'），
    而普通 import 的单测对此无感（模块在 sys.modules 里）。这里复刻 loader 的
    装载方式，并断言"exec 即生效"（订阅 + 登记 handler 都已就位）。
    """
    import importlib.util

    monkeypatch.setenv(mirror_mod.HOME_ENV, str(tmp_path / "claude"))
    assert mirror_mod.__file__ is not None
    spec = importlib.util.spec_from_file_location(
        "wing_hook_claude_session_mirror_regression", mirror_mod.__file__
    )
    assert spec is not None and spec.loader is not None
    loaded = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(loaded)  # 不注册 sys.modules = loader 的原样行为
        assert getattr(loaded, "_ACTIVE", None) is not None, "exec 即应装上投影器"
        assert loaded._on_before_session_start in hooks.handlers("before_session_start")
    finally:
        loaded.uninstall()
        hooks.off("before_session_start", loaded._on_before_session_start)
    assert event_bus.subscriber_count == 0


def test_register_into_custom_registry(tmp_path):
    """手动注册路径：``register_claude_session_mirror`` + 自家 registry.invoke 闭环。"""
    registry = HookRegistry()
    mirror_mod.register_claude_session_mirror(registry)

    bus = EventBus()
    home = tmp_path / "claude"
    project = tmp_path / "registry project"
    project.mkdir()
    instance = mirror_mod.install(bus=bus, claude_home=home)
    assert instance is not None
    try:
        session = type(
            "FakeSession",
            (),
            {"session_id": SESSION_ID, "session_workspace": str(project)},
        )()
        registry.invoke("before_session_start", session)
        bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))
        assert instance.flush()
        assert _project_file(home, str(project.resolve())).is_file()
    finally:
        mirror_mod.uninstall(bus)


def test_cwd_from_metadata_fallback(monkeypatch, tmp_path):
    """resume 路径（无 before_session_start、无订阅事件）靠 metadata 兜底。"""
    sessions_root = tmp_path / "sessions"
    (sessions_root / SESSION_ID).mkdir(parents=True)
    project = tmp_path / "resumed project"
    project.mkdir()
    (sessions_root / SESSION_ID / "metadata.json").write_text(
        json.dumps({"workspace": str(project)}), encoding="utf-8"
    )
    monkeypatch.setenv("WING_SESSIONS_PATH", str(sessions_root))

    bus = EventBus()
    home = tmp_path / "claude"
    instance = mirror_mod.install(bus=bus, claude_home=home)
    assert instance is not None
    try:
        bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))
        assert instance.flush()
        assert _project_file(home, str(project.resolve())).is_file()
    finally:
        mirror_mod.uninstall(bus)


def test_rows_parked_until_cwd_is_known(mirror, project):
    """cwd 未知时挂起而不猜目录；cwd 到达后按序补写。"""
    instance, bus, home = mirror
    bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))
    assert instance.flush()
    assert not (home / "projects").exists(), "cwd 未知时不得写任何投影"

    instance.note_session(SESSION_ID, project)
    assert instance.flush()

    rows = _read_rows(_project_file(home, project))
    assert [row["type"] for row in rows] == ["user", "last-prompt"]
    assert rows[0]["uuid"] and rows[0]["parentUuid"] is None


# ============================================================
# 跨进程续链（网关重启）与写盘失败（R1: S1 / S2）
# ============================================================


def _fresh_mirror(home: Path):
    """新 EventBus + 新投影器实例：模拟进程重启（内存态全丢，磁盘文件还在）。"""
    bus = EventBus()
    instance = mirror_mod.install(bus=bus, claude_home=home)
    assert instance is not None
    return instance, bus


def _chained_rows(rows: list[dict]) -> list[dict]:
    return [row for row in rows if "uuid" in row]


def _assert_single_root_chain(rows: list[dict]) -> None:
    """全文件结构自检：恰好一个根、无悬空 parentUuid、链序与行序一致。"""
    chained = _chained_rows(rows)
    uuids = {row["uuid"] for row in chained}
    roots = [row["uuid"] for row in chained if row["parentUuid"] is None]
    assert len(roots) == 1, f"投影文件必须单根（实际根数 {len(roots)}）"
    for row in chained:
        assert row["parentUuid"] is None or row["parentUuid"] in uuids, (
            f"悬空 parentUuid: {row}"
        )
    for previous, current in zip(chained, chained[1:], strict=False):
        assert current["parentUuid"] == previous["uuid"]


def test_seed_last_uuid_reads_tail_and_skips_junk(tmp_path):
    """游标种子：空 / 缺失 / 只有标题行 → None；半截行与非法行跳过；超长尾行放大窗口。"""
    seed = mirror_mod._seed_last_uuid
    assert seed(tmp_path / "missing.jsonl") is None

    empty = tmp_path / "empty.jsonl"
    empty.write_text("", encoding="utf-8")
    assert seed(empty) is None

    title_only = tmp_path / "title.jsonl"
    title_only.write_text(
        json.dumps({"type": "custom-title", "customTitle": "t"}) + "\n",
        encoding="utf-8",
    )
    assert seed(title_only) is None

    rows = [
        {"type": "user", "uuid": "u1"},
        {"type": "last-prompt", "leafUuid": "u1"},
        {"type": "assistant", "uuid": "a1"},
    ]
    normal = tmp_path / "normal.jsonl"
    normal.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
    assert seed(normal) == "a1"

    # 尾部半截行（崩溃残留、无换行）→ 回退到最后一条完整行
    torn = tmp_path / "torn.jsonl"
    torn.write_text(
        "".join(json.dumps(row) + "\n" for row in rows)
        + '{"type": "user", "uuid": "torn',
        encoding="utf-8",
    )
    assert seed(torn) == "a1"

    # 末行超长（> SEED_WINDOW_START）→ 放大窗口后仍要找到它
    huge = tmp_path / "huge.jsonl"
    huge.write_text(
        json.dumps({"type": "user", "uuid": "u-before"})
        + "\n"
        + json.dumps({"type": "assistant", "uuid": "a-huge", "text": "x" * 200_000})
        + "\n",
        encoding="utf-8",
    )
    assert seed(huge) == "a-huge"

    # 末行是 last-prompt → 取 leafUuid（真实转录的续链标记）
    tail_prompt = tmp_path / "tail-prompt.jsonl"
    tail_prompt.write_text(
        json.dumps({"type": "user", "uuid": "u9"})
        + "\n"
        + json.dumps({"type": "last-prompt", "lastPrompt": "p", "leafUuid": "u9"})
        + "\n",
        encoding="utf-8",
    )
    assert seed(tail_prompt) == "u9"

    # 残行判据（决定首次落盘要不要先补换行）
    assert mirror_mod._ends_mid_line(tmp_path / "missing.jsonl") is False
    assert mirror_mod._ends_mid_line(empty) is False
    assert mirror_mod._ends_mid_line(normal) is False
    assert mirror_mod._ends_mid_line(torn) is True


def test_restart_resumes_chain_from_last_prompt_leaf(tmp_path, project):
    """网关重启后续链（末行是 last-prompt 行 → 种子取 leafUuid），全文件单根。"""
    home = tmp_path / "claude"
    first, bus1 = _fresh_mirror(home)
    try:
        first.note_session(SESSION_ID, project)
        bus1.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="before"))
        assert first.flush()
    finally:
        mirror_mod.uninstall(bus1)

    second, bus2 = _fresh_mirror(home)
    try:
        second.note_session(SESSION_ID, project)
        bus2.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="after"))
        assert second.flush()
    finally:
        mirror_mod.uninstall(bus2)

    rows = _read_rows(_project_file(home, project))
    assert [row["type"] for row in rows] == [
        "user",
        "last-prompt",
        "user",
        "last-prompt",
    ]
    assert rows[2]["parentUuid"] == rows[0]["uuid"], "重启后的首行必须续到旧链上"
    _assert_single_root_chain(rows)


def test_restart_resumes_chain_from_last_row_uuid(tmp_path, project):
    """末行是 assistant 行时，种子取该行自身的 uuid。"""
    home = tmp_path / "claude"
    first, bus1 = _fresh_mirror(home)
    try:
        first.note_session(SESSION_ID, project)
        bus1.emit(_assistant_event(text="before restart"))
        assert first.flush()
    finally:
        mirror_mod.uninstall(bus1)

    last_before = _read_rows(_project_file(home, project))[-1]

    second, bus2 = _fresh_mirror(home)
    try:
        second.note_session(SESSION_ID, project)
        bus2.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="after"))
        assert second.flush()
    finally:
        mirror_mod.uninstall(bus2)

    rows = _read_rows(_project_file(home, project))
    new_user = [row for row in rows if row["type"] == "user"][0]
    assert new_user["parentUuid"] == last_before["uuid"]
    _assert_single_root_chain(rows)


def test_restart_skips_torn_tail_line(tmp_path, project):
    """文件尾部有半截行时，种子回退到最后一条完整行（链不悬空）。"""
    home = tmp_path / "claude"
    first, bus1 = _fresh_mirror(home)
    try:
        first.note_session(SESSION_ID, project)
        bus1.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="before"))
        assert first.flush()
    finally:
        mirror_mod.uninstall(bus1)

    path = _project_file(home, project)
    with path.open("a", encoding="utf-8") as handle:
        handle.write('{"type": "user", "uuid": "half-writ')  # 半截、无换行

    second, bus2 = _fresh_mirror(home)
    try:
        second.note_session(SESSION_ID, project)
        bus2.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="after"))
        assert second.flush()
    finally:
        mirror_mod.uninstall(bus2)

    parsed: list[dict] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            parsed.append(json.loads(line))
        except ValueError:
            continue  # 半截行（我们自己的写入不会产生，只有崩溃残留）
    new_user = [row for row in parsed if row.get("type") == "user"][-1]
    assert new_user["parentUuid"] == parsed[0]["uuid"]
    _assert_single_root_chain(parsed)


def test_write_failure_does_not_advance_cursor(tmp_path, project):
    """写盘失败的一批不推进游标：恢复后的首行 parentUuid 指向文件里真实存在的行。"""
    home = tmp_path / "claude"
    instance, bus = _fresh_mirror(home)
    try:
        instance.note_session(SESSION_ID, project)
        bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="first"))
        assert instance.flush()

        path = _project_file(home, project)
        backup = path.with_name(f"{path.name}.bak")
        path.rename(backup)
        path.mkdir()  # 同名目录：追加写必然失败（可移植的注入，不依赖权限位）
        bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="lost"))
        assert instance.flush()
        assert path.is_dir(), "注入未生效"
        assert json.loads(backup.read_text(encoding="utf-8").splitlines()[0])[
            "uuid"
        ]  # 备份里只有第一批

        path.rmdir()
        backup.rename(path)
        bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="recovered"))
        assert instance.flush()
    finally:
        mirror_mod.uninstall(bus)

    rows = _read_rows(path)
    assert [row["type"] for row in rows] == [
        "user",
        "last-prompt",
        "user",
        "last-prompt",
    ]
    assert all(
        row["message"]["content"][0]["text"] != "lost"
        for row in rows
        if row["type"] == "user"
    ), "写失败的批次不得落盘"
    recovered = [row for row in rows if row["type"] == "user"][-1]
    assert recovered["parentUuid"] == rows[0]["uuid"], (
        "恢复后的首行必须指向最后一个**已落盘**行"
    )
    _assert_single_root_chain(rows)


# ============================================================
# 内容上限安全网（design D7 / R1: N2）
# ============================================================


def test_cap_boundary_exact_limit_and_one_over():
    """`_cap`：恰好等于上限不截断；超 1 字符即截断并带可见标记。"""
    assert mirror_mod._cap("x" * 10, 10) == "x" * 10
    capped = mirror_mod._cap("x" * 11, 10)
    assert capped.startswith("x" * 10)
    assert "truncated by wing claude mirror" in capped
    assert "original 11 chars" in capped
    assert mirror_mod._cap("", 10) == ""


def test_content_and_last_prompt_limits_in_rows(mirror, project):
    """集成口径：>1 MiB 的块被截断；`lastPrompt` 服从 LAST_PROMPT_MAX_CHARS。"""
    instance, bus, home = mirror
    instance.note_session(SESSION_ID, project)
    bus.emit(
        UserMessageAcceptedEvent(
            session_id=SESSION_ID, content="y" * (mirror_mod.MAX_CONTENT_CHARS + 5)
        )
    )
    assert instance.flush()

    rows = _read_rows(_project_file(home, project))
    user, last_prompt = rows[0], rows[1]
    text = user["message"]["content"][0]["text"]
    assert "original" in text and "truncated by wing claude mirror" in text
    assert len(text) < mirror_mod.MAX_CONTENT_CHARS + 200

    assert last_prompt["lastPrompt"].startswith("y" * 100)
    assert "truncated by wing claude mirror" in last_prompt["lastPrompt"]
    assert len(last_prompt["lastPrompt"]) <= mirror_mod.LAST_PROMPT_MAX_CHARS + 200


def test_queue_full_drops_record_without_raising(monkeypatch, tmp_path):
    """队列满即丢弃并计数（宁可少写不可积压），且不向调用方抛。"""
    bus = EventBus()
    instance = mirror_mod.install(bus=bus, claude_home=tmp_path / "claude")
    assert instance is not None
    try:
        monkeypatch.setattr(instance, "_ensure_worker", lambda: None)  # 冻结消费者
        instance._queue = mirror_mod.queue.Queue(maxsize=1)
        event = UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi")
        assert instance._enqueue(("event", event)) is True
        assert instance._enqueue(("event", event)) is False
        assert instance._dropped == 1
    finally:
        mirror_mod.uninstall(bus)


def test_handler_error_is_isolated(mirror):
    """回调内部异常就地捕获：绝不向 EventBus（agent 主流程）抛。"""
    instance, bus, _home = mirror

    def _boom(_item):
        raise RuntimeError("boom")

    instance._enqueue = _boom  # type: ignore[method-assign]
    instance._on_event(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))


# ============================================================
# 幂等订阅
# ============================================================


def test_install_subscribes_global_event_bus(monkeypatch, tmp_path):
    """默认安装（模块装载 / hook 文件 exec 的路径）挂在全局 event_bus 上。"""
    monkeypatch.setenv(mirror_mod.HOME_ENV, str(tmp_path / "claude"))
    instance = mirror_mod.install()
    try:
        assert instance is not None
        assert getattr(event_bus, mirror_mod._MARKER, None) is instance
        assert event_bus.subscriber_count == 1
    finally:
        mirror_mod.uninstall()
    assert event_bus.subscriber_count == 0


def test_install_is_idempotent_on_same_bus(tmp_path):
    """重复 install（reload / 多次 import）只保留一个订阅者与一个写线程。"""
    bus = EventBus()
    home = tmp_path / "claude"
    first = mirror_mod.install(bus=bus, claude_home=home)
    try:
        assert bus.subscriber_count == 1
        second = mirror_mod.install(bus=bus, claude_home=home)
        assert second is first
        assert bus.subscriber_count == 1
    finally:
        mirror_mod.uninstall(bus)
    assert bus.subscriber_count == 0
    assert getattr(bus, mirror_mod._MARKER, None) is None


def test_reinstall_does_not_duplicate_rows(tmp_path):
    """卸载 → 重装 → 事件只写一行（订阅不重复、行不重复）。"""
    bus = EventBus()
    home = tmp_path / "claude"
    project = tmp_path / "proj"
    project.mkdir()
    mirror_before = mirror_mod.install(bus=bus, claude_home=home)
    mirror_mod.uninstall(bus)

    instance = mirror_mod.install(bus=bus, claude_home=home)
    assert instance is not None
    assert instance is not mirror_before
    try:
        instance.note_session(SESSION_ID, str(project))
        bus.emit(UserMessageAcceptedEvent(session_id=SESSION_ID, content="hi"))
        assert instance.flush()
        rows = _read_rows(_project_file(home, str(project.resolve())))
        assert [row["type"] for row in rows] == ["user", "last-prompt"]
    finally:
        mirror_mod.uninstall(bus)


def test_install_disabled_unsubscribes(monkeypatch, tmp_path):
    """ENABLED=False → 卸载且不再订阅（停用开关）。"""
    bus = EventBus()
    mirror_mod.install(bus=bus, claude_home=tmp_path / "claude")
    assert bus.subscriber_count == 1
    monkeypatch.setattr(mirror_mod, "ENABLED", False)
    assert mirror_mod.install(bus=bus) is None
    assert bus.subscriber_count == 0
