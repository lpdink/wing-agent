"""SessionStore 与 MessageLog 的单元测试：file / memory 两个后端。"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

from wing.store import (
    FileSessionStore,
    MemorySessionStore,
    SessionMetadata,
    SessionStore,
)


def _sid(label: str) -> str:
    """测试用 session id：稳定、唯一、契合后端生成的既定格式。

    存储层把 session id 当路径组件并校验格式（防路径穿越的最终防线）——
    'sid-1' 这类任意字符串不再是合法的存储键。
    """
    digest = hashlib.md5(label.encode()).hexdigest()[:8]
    return f"20250101-000000-{digest}"


@pytest.fixture(params=["file", "memory"])
def store(request: pytest.FixtureRequest, tmp_path: Path) -> SessionStore:
    if request.param == "file":
        return FileSessionStore(tmp_path / "sessions")
    return MemorySessionStore()


class TestMetadata:
    def test_roundtrip_full(self, store: SessionStore):
        meta = SessionMetadata(
            session_name="hello",
            workspace="/tmp/ws",
            last_interaction="2026-07-25T22:00:00",
            forked_from="20260101-000000-aaaaaaaa",
            template_name="coder",
            model_name="qwen3-max",
            provider_name="dashscope",
        )
        store.save_metadata(_sid("1"), meta)
        loaded = store.load_metadata(_sid("1"))
        assert loaded == meta

    def test_model_binding_roundtrip(self, store: SessionStore):
        """模型绑定成对 round-trip：provider 与 model 一并读回。"""
        store.save_metadata(
            _sid("model"),
            SessionMetadata(model_name="qwen3-max", provider_name="dashscope"),
        )
        loaded = store.load_metadata(_sid("model"))
        assert loaded is not None
        assert loaded.model_name == "qwen3-max"
        assert loaded.provider_name == "dashscope"

    def test_half_written_model_binding_kept_as_is(self, store: SessionStore):
        """半写记录原样存取（「单字段视为无记录」是读取侧语义，非存储侧）。"""
        store.save_metadata(_sid("half"), SessionMetadata(model_name="qwen3-max"))
        loaded = store.load_metadata(_sid("half"))
        assert loaded is not None
        assert loaded.model_name == "qwen3-max"
        assert loaded.provider_name is None

    def test_load_missing_returns_none(self, store: SessionStore):
        assert store.load_metadata(_sid("no-such")) is None

    def test_save_empty_is_no_record(self, store: SessionStore):
        store.save_metadata(_sid("2"), SessionMetadata())
        assert store.load_metadata(_sid("2")) is None

    def test_partial_fields(self, store: SessionStore):
        store.save_metadata(_sid("3"), SessionMetadata(workspace="/tmp/ws"))
        loaded = store.load_metadata(_sid("3"))
        assert loaded is not None
        assert loaded.workspace == "/tmp/ws"
        assert loaded.session_name is None
        assert loaded.forked_from is None
        assert loaded.model_name is None
        assert loaded.provider_name is None


class TestFileCompat:
    """文件后端专属：老格式与未知字段兼容。"""

    def test_legacy_metadata_unknown_fields(self, tmp_path: Path):
        root = tmp_path / "sessions"
        sid = "20260101-000000-aaaaaaaa"
        session_dir = root / sid
        session_dir.mkdir(parents=True)
        (session_dir / "metadata.json").write_text(
            json.dumps({"workspace": "/old/ws", "some_future_field": 42}),
            encoding="utf-8",
        )
        store = FileSessionStore(root)
        loaded = store.load_metadata(sid)
        assert loaded is not None
        assert loaded.workspace == "/old/ws"
        assert loaded.session_name is None

    def test_corrupted_metadata_returns_empty(self, tmp_path: Path):
        root = tmp_path / "sessions"
        session_dir = root / _sid("bad")
        session_dir.mkdir(parents=True)
        (session_dir / "metadata.json").write_text("{not json", encoding="utf-8")
        store = FileSessionStore(root)
        loaded = store.load_metadata(_sid("bad"))
        assert loaded == SessionMetadata()


class TestMessageLog:
    def test_append_load_order(self, store: SessionStore):
        log = store.open_log(_sid("log"))
        records = [
            {"role": "user", "content": f"m{i}", "uuid": f"u{i}"} for i in range(5)
        ]
        log.append(records[:2])
        log.append(records[2:])
        assert list(log.iter_all()) == records

    def test_append_empty_is_nop(self, store: SessionStore):
        log = store.open_log(_sid("empty"))
        log.append([])
        assert list(log.iter_all()) == []

    def test_open_log_same_handle_content(self, store: SessionStore):
        log1 = store.open_log(_sid("shared"))
        log1.append([{"role": "user", "content": "x"}])
        log2 = store.open_log(_sid("shared"))
        assert len(list(log2.iter_all())) == 1


class TestFileLogStreaming:
    """file 后端的流式读契约（加载路径据此把 MiB 级历史的装载峰值降下来）。"""

    def test_iter_all_skips_blank_and_corrupted_lines(self, tmp_path: Path):
        from wing.store.file import FileMessageLog

        log = FileMessageLog(tmp_path)
        log.append([{"role": "user", "content": "ok"}])
        hist = tmp_path / "history.jsonl"
        hist.write_text(
            hist.read_text(encoding="utf-8")
            + "\n{not json}\n"
            + json.dumps({"role": "assistant", "content": "tail"})
            + "\n",
            encoding="utf-8",
        )
        assert [r["content"] for r in log.iter_all()] == ["ok", "tail"]

    def test_iter_all_skips_non_dict_json(self, tmp_path: Path):
        """能解析但不是 dict 的行同样跳过——契约是"记录是 dict"。

        放行会让消费方在 ``record.get`` 上炸掉：fork 切片、标题回退
        （进而整个 session 列表接口）都直接吃这个契约。
        """
        from wing.store.file import FileMessageLog

        log = FileMessageLog(tmp_path)
        log.append([{"role": "user", "content": "ok"}])
        hist = tmp_path / "history.jsonl"
        hist.write_text(
            hist.read_text(encoding="utf-8") + '123\n"abc"\n[1, 2]\n',
            encoding="utf-8",
        )
        assert [r["content"] for r in log.iter_all()] == ["ok"]

    def test_iter_all_parses_lazily(self, tmp_path: Path, monkeypatch):
        """取第一条只解析第一条——不得预读/物化整份文件。"""
        import wing.store.file as file_module
        from types import SimpleNamespace
        from wing.store.file import FileMessageLog

        log = FileMessageLog(tmp_path)
        log.append([{"role": "user", "content": f"m{i}"} for i in range(5)])

        parsed = 0
        real_loads = json.loads

        def counting_loads(line: str, *args, **kwargs):
            nonlocal parsed
            parsed += 1
            return real_loads(line, *args, **kwargs)

        # 只替换本模块的 json 引用（全局 json 模块不被动）
        monkeypatch.setattr(file_module, "json", SimpleNamespace(loads=counting_loads))
        iterator = log.iter_all()
        assert next(iterator)["content"] == "m0"
        assert parsed == 1

    def test_append_serializes_before_writing(self, tmp_path: Path):
        """整批序列化/编码先于任何写入：失败不留半批、不创建文件。

        孤立代理字符（provider 响应的非法转义经 json.loads 原样产出）是
        现实入口：编码必须发生在 open 之前，否则 fork 的前缀批量追加会留下
        "有历史没 metadata"的幽灵会话。
        """
        from wing.store.file import FileMessageLog

        log = FileMessageLog(tmp_path)
        log.append([{"role": "user", "content": "keep"}])
        before = (tmp_path / "history.jsonl").read_bytes()

        with pytest.raises(UnicodeEncodeError):
            log.append([{"role": "user", "content": "ok"}, {"content": "\ud800"}])

        assert (tmp_path / "history.jsonl").read_bytes() == before
        assert [r["content"] for r in log.iter_all()] == ["keep"]

    def test_append_failure_does_not_create_file(self, tmp_path: Path):
        from wing.store.file import FileMessageLog

        log = FileMessageLog(tmp_path / "fresh")
        with pytest.raises(TypeError):
            log.append([{"role": "user", "content": {1}}])
        assert not (tmp_path / "fresh" / "history.jsonl").exists()


class TestAux:
    def test_write_read_delete(self, store: SessionStore):
        log = store.open_log(_sid("aux"))
        assert log.read_aux("pending_compact") is None
        log.write_aux("pending_compact", {"start_uuid": "a"})
        assert log.read_aux("pending_compact") == {"start_uuid": "a"}
        log.delete_aux("pending_compact")
        assert log.read_aux("pending_compact") is None

    def test_delete_missing_is_nop(self, store: SessionStore):
        log = store.open_log(_sid("aux2"))
        log.delete_aux("never-existed")

    def test_file_corrupted_aux_discarded(self, tmp_path: Path):
        from wing.store.file import FileMessageLog

        log = FileMessageLog(tmp_path / "sid-corrupt")
        log._path.mkdir(parents=True)
        (log._path / "pending_compact.json").write_text("{bad", encoding="utf-8")
        assert log.read_aux("pending_compact") is None
        assert not (log._path / "pending_compact.json").exists()


class TestExists:
    def _seed(self, store: SessionStore, *sids: str):
        for sid in sids:
            store.save_metadata(sid, SessionMetadata(session_name=sid))

    def test_exact_match(self, store: SessionStore):
        self._seed(store, "20260101-111111-aaaaaaaa")
        assert store.exists("20260101-111111-aaaaaaaa") is True

    def test_missing_returns_false(self, store: SessionStore):
        self._seed(store, "20260101-111111-aaaaaaaa")
        assert store.exists(_sid("missing")) is False

    def test_no_fuzzy_matching(self, store: SessionStore):
        """精确匹配：前缀 / 子串 / 通配符都不解析。

        存储层的 id 契约是**闸门**：真正危险的值（穿越 / 点开头 / 控制字符 /
        超长）在入口即 ValueError，绝不进路径拼接；而任意**安全**字符串
        （id 不透明——编排方可自带 UUID 等）一律合法，只是"不存在" → False。
        "不匹配"与"非法输入"是两种拒绝（前者 False、后者 raise）；网络侧
        统一表现为 404（会话层解析闸门先把非法 id 折成"不存在"）。
        """
        self._seed(store, "20260101-111111-aaaaaaaa", "20260202-222222-bbbbbbbb")
        for malformed in ("../20260101", "20260101/111111", ".hidden", "a..b", "\x00"):
            with pytest.raises(ValueError):
                store.exists(malformed)
        # 合法（含非既定时态形态：前缀 / 子串 / 通配符 / 任意编排方 id）但不存在
        # → False（精确匹配，不做前缀解析）
        for safe in ("20260101", "bbbbbbbb", "20260101*aaaaaaaa", "zzz"):
            assert store.exists(safe) is False
        assert store.exists("20260101-111111-bbbbbbbb") is False

    def test_empty_session_not_exists(self, store: SessionStore):
        """仅 open_log 而未写入任何记录的 session 不算存在（两后端一致）。"""
        store.open_log(_sid("empty"))
        assert store.exists(_sid("empty")) is False
        store.open_log(_sid("empty")).append([{"role": "user", "content": "x"}])
        assert store.exists(_sid("empty")) is True


class TestListSummaries:
    def test_requires_messages_or_tags(self, store: SessionStore):
        # 只有 metadata（不带标签）又没有消息 → 不列出
        store.save_metadata(_sid("meta-only"), SessionMetadata(session_name="x"))
        assert store.list_summaries() == []

    def test_tagged_session_without_messages_is_listed(self, store: SessionStore):
        """带标签的会话即使还没有首条消息也列出（"创建即打标"的窗口期可查）。"""
        sid = _sid("tagged")
        store.save_metadata(sid, SessionMetadata(tags=["favorite"]))
        summaries = store.list_summaries()
        assert [s.id for s in summaries] == [sid]
        assert summaries[0].metadata.tags == ["favorite"]
        assert summaries[0].first_user_message is None

    def test_name_from_metadata(self, store: SessionStore):
        store.save_metadata(_sid("1"), SessionMetadata(session_name="titled"))
        store.open_log(_sid("1")).append([{"role": "user", "content": "first"}])
        summaries = store.list_summaries()
        assert len(summaries) == 1
        assert summaries[0].id == _sid("1")
        assert summaries[0].metadata.session_name == "titled"
        assert summaries[0].first_user_message is None

    def test_first_user_message_fallback(self, store: SessionStore):
        store.save_metadata(_sid("2"), SessionMetadata(workspace="/ws"))
        store.open_log(_sid("2")).append(
            [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello " * 30},
            ]
        )
        summaries = store.list_summaries()
        assert len(summaries) == 1
        assert summaries[0].metadata.session_name is None
        assert summaries[0].first_user_message is not None
        assert len(summaries[0].first_user_message) <= 100
        assert summaries[0].first_user_message.startswith("hello")


class TestMemoryNoDisk:
    """memory 后端的任何操作都不产生文件。"""

    def test_full_lifecycle_no_files(self, tmp_path: Path):
        sentinel = tmp_path / "should-never-exist"
        store = MemorySessionStore()

        store.save_metadata(
            _sid("m"), SessionMetadata(session_name="m", workspace="/w")
        )
        log = store.open_log(_sid("m"))
        log.append([{"role": "user", "content": "hi", "uuid": "u1"}])
        log.write_aux("pending_compact", {"start_uuid": "u1"})
        assert log.read_aux("pending_compact") is not None
        assert store.list_summaries()
        assert store.exists(_sid("m")) is True

        assert not sentinel.exists()
        # tmp_path 下没有任何 wing 产生的内容
        assert list(tmp_path.iterdir()) == []


class TestFileLayout:
    """文件后端磁盘布局：每会话一个目录，重放只靠 history.jsonl（无 newest.json 快照）。"""

    def test_layout_files(self, tmp_path: Path):
        root = tmp_path / "sessions"
        store = FileSessionStore(root)
        store.save_metadata(
            _sid("l"), SessionMetadata(session_name="l", workspace="/w")
        )
        log = store.open_log(_sid("l"))
        log.append([{"role": "user", "content": "hi"}])
        log.write_aux("pending_compact", {"k": "v"})

        session_dir = root / _sid("l")
        assert (session_dir / "metadata.json").exists()
        assert (session_dir / "history.jsonl").exists()
        assert not (session_dir / "newest.json").exists()
        assert (session_dir / "pending_compact.json").exists()

        meta = json.loads((session_dir / "metadata.json").read_text())
        assert meta == {"session_name": "l", "workspace": "/w"}

        lines = (session_dir / "history.jsonl").read_text().strip().splitlines()
        assert len(lines) == 1
        assert json.loads(lines[0])["content"] == "hi"

    def test_history_append_only(self, tmp_path: Path):
        store = FileSessionStore(tmp_path / "sessions")
        log = store.open_log(_sid("a"))
        log.append([{"role": "user", "content": "1"}])
        log.append([{"role": "assistant", "content": "2"}])
        lines = (tmp_path / "sessions" / _sid("a") / "history.jsonl").read_text()
        assert lines.count("\n") == 2


class TestListSummariesResilience:
    """坏 metadata（合法 JSON / 非法 schema）不能噎死整个列表（r2 复核 S-1 回归）。"""

    def test_schema_invalid_metadata_does_not_break_listing(self, tmp_path: Path):
        """no-history 目录：不再因先读 metadata 而抛 ValidationError。"""
        root = tmp_path / "sessions"
        bad = root / _sid("schema-bad")
        bad.mkdir(parents=True)
        (bad / "metadata.json").write_text(
            json.dumps({"tags": "abc"}), encoding="utf-8"
        )

        store = FileSessionStore(root)
        assert store.list_summaries() == []  # 降级为"无 metadata"，而不是 raise

    def test_schema_invalid_metadata_with_history_still_listed(self, tmp_path: Path):
        """带 history 的坏 metadata：按"损坏数据 warning + 空"政策降级，条目保留。"""
        root = tmp_path / "sessions"
        sid = _sid("schema-bad-history")
        session_dir = root / sid
        session_dir.mkdir(parents=True)
        (session_dir / "history.jsonl").write_text(
            json.dumps({"role": "user", "content": "hello", "uuid": "u1"}) + "\n",
            encoding="utf-8",
        )
        (session_dir / "metadata.json").write_text(
            json.dumps({"tags": 42}), encoding="utf-8"
        )

        store = FileSessionStore(root)
        summaries = store.list_summaries()
        assert [s.id for s in summaries] == [sid]
        assert summaries[0].metadata.tags is None
        assert summaries[0].first_user_message == "hello"
