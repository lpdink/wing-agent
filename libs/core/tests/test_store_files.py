"""用户文件存储面（``SessionStore.write_file`` / 文件名闸门）单元测试。

各层各测自己那一层的契约：

- 名字闸门（``validate_file_name`` / ``is_valid_file_name``）：接受面（中文 / 空格 /
  ``-`` 开头 / 长名边界）与拒绝面（空 / 点开头 / 路径分隔符 / ``..`` / 控制符 / 超长
  字节 / 非 UTF-8 可编码），错误类型是 ``InvalidInputError``（HTTP 400 语义）；
- file 后端：落点 ``<root>/<sid>/files/<名>``、同名同字节**幂等不重写**、同名异字节
  ``stem-2.ext`` 递增（含复用已存在的同字节候选名）、加后缀后超长即拒绝（不静默截断）；
- memory 后端：同一套命名语义，``FileRef.path`` 恒 None（没有本地文件）。

整机链路（端点 → 磁盘 → agent Read 读得动）由 probe 场景
``libs/wing-probe/scenarios/test_session_files_upload.py`` 钉住。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from wing.common.utils import InvalidInputError
from wing.store import FileRef, FileSessionStore, MemorySessionStore
from wing.store.base import (
    FILE_NAME_MAX_BYTES,
    file_too_large_message,
    is_valid_file_name,
    validate_file_name,
)


# ── 1. 名字闸门 ────────────────────────────────────────────────


class TestFileNameGate:
    @pytest.mark.parametrize(
        "name",
        [
            "notes.txt",
            "报告.pdf",  # 中文合法（按字节计长）
            "no-extension",
            "archive.tar.gz",
            "-leading-dash",
            "with space.txt",
            "a" * FILE_NAME_MAX_BYTES,  # 恰好到字节上限
            "收" * 42 + "x",  # 126 + 1 = 127 字节
        ],
        ids=[
            "plain",
            "cjk",
            "no-ext",
            "multi-ext",
            "leading-dash",
            "space",
            "max-bytes",
            "cjk-near-max",
        ],
    )
    def test_accepts_and_returns_unchanged(self, name: str) -> None:
        assert is_valid_file_name(name)
        assert validate_file_name(name) == name

    @pytest.mark.parametrize(
        "name",
        [
            "",
            ".",
            "..",
            ".hidden",  # 点开头：保留点命名空间 / 隐藏文件歧义
            "../escape",
            "a/b",
            "a\\b",
            "x..y",
            "a\x00b",
            "a\nb",
            "a" * (FILE_NAME_MAX_BYTES + 1),
            "收" * 43,  # 129 字节：按**字节**超限（按字符只有 43）
            "\ud800",  # 孤立代理字符：合法 JSON 但不是合法 UTF-8
        ],
        ids=[
            "empty",
            "dot",
            "dotdot",
            "leading-dot",
            "parent-ref",
            "slash",
            "backslash",
            "embedded-dotdot",
            "nul",
            "newline",
            "too-long-ascii",
            "too-long-cjk-bytes",
            "lone-surrogate",
        ],
    )
    def test_rejects(self, name: str) -> None:
        assert not is_valid_file_name(name)
        with pytest.raises(InvalidInputError):
            validate_file_name(name)

    @pytest.mark.parametrize("value", [None, 7, b"bytes"], ids=["none", "int", "bytes"])
    def test_non_string_values_are_rejected(self, value: object) -> None:
        assert not is_valid_file_name(value)

    def test_rejection_message_escapes_the_name(self) -> None:
        """错误文案里的名字走 ``repr``——代理字符不会在 HTTP 错误体序列化处再炸。"""
        with pytest.raises(InvalidInputError) as excinfo:
            validate_file_name("\ud800")
        assert "'\\ud800'" in str(excinfo.value)

    def test_size_message_names_the_config_key(self) -> None:
        assert file_too_large_message(100 * 1024 * 1024, 64 * 1024 * 1024) == (
            "file too large: 100.0 MB exceeds the 64.0 MB per-file limit "
            "(config files.max_bytes)"
        )


# ── 2. file 后端 ───────────────────────────────────────────────


@pytest.fixture
def store(tmp_path: Path) -> FileSessionStore:
    return FileSessionStore(tmp_path / "sessions")


SESSION = "store-files-1"


def files_dir(store: FileSessionStore, session_id: str = SESSION) -> Path:
    return store.root / session_id / "files"


class TestFileStoreWriteFile:
    def test_writes_bytes_to_the_session_files_dir(
        self, store: FileSessionStore
    ) -> None:
        ref = store.write_file(SESSION, "notes.txt", b"hello")

        assert ref == FileRef(
            name="notes.txt",
            path=str(files_dir(store) / "notes.txt"),
            bytes=5,
        )
        assert (files_dir(store) / "notes.txt").read_bytes() == b"hello"
        assert sorted(p.name for p in files_dir(store).iterdir()) == ["notes.txt"]

    def test_same_name_same_bytes_is_idempotent(self, store: FileSessionStore) -> None:
        first = store.write_file(SESSION, "notes.txt", b"same")
        before = Path(str(first.path)).stat()

        second = store.write_file(SESSION, "notes.txt", b"same")

        assert second == first  # 同名同字节：返回既有引用，不产生第二份
        after = Path(str(second.path)).stat()
        assert (after.st_ino, after.st_mtime_ns) == (
            before.st_ino,
            before.st_mtime_ns,
        ), "幂等命中重写了文件"
        assert sorted(p.name for p in files_dir(store).iterdir()) == ["notes.txt"]

    def test_same_name_different_bytes_gets_a_suffix(
        self, store: FileSessionStore
    ) -> None:
        first = store.write_file(SESSION, "report.pdf", b"one")
        second = store.write_file(SESSION, "report.pdf", b"two")
        third = store.write_file(SESSION, "report.pdf", b"three")

        assert (first.name, second.name, third.name) == (
            "report.pdf",
            "report-2.pdf",
            "report-3.pdf",
        )
        assert (files_dir(store) / "report-2.pdf").read_bytes() == b"two"
        assert (files_dir(store) / "report-3.pdf").read_bytes() == b"three"
        # 首次那份没被覆盖（链上的 [file: …] 引用指向它）。
        assert (files_dir(store) / "report.pdf").read_bytes() == b"one"

    def test_suffixed_candidate_with_same_bytes_is_reused(
        self, store: FileSessionStore
    ) -> None:
        """被顶掉过的那份内容再次上传：复用 ``-2`` 候选，不产生第三份。"""
        store.write_file(SESSION, "notes.txt", b"first")
        store.write_file(SESSION, "notes.txt", b"second")

        again = store.write_file(SESSION, "notes.txt", b"second")

        assert again.name == "notes-2.txt"
        assert sorted(p.name for p in files_dir(store).iterdir()) == [
            "notes-2.txt",
            "notes.txt",
        ]

    def test_multi_extension_and_extensionless_names(
        self, store: FileSessionStore
    ) -> None:
        store.write_file(SESSION, "archive.tar.gz", b"a")
        assert store.write_file(SESSION, "archive.tar.gz", b"b").name == (
            "archive.tar-2.gz"
        )
        store.write_file(SESSION, "notes", b"a")
        assert store.write_file(SESSION, "notes", b"b").name == "notes-2"

    def test_suffix_that_overflows_the_byte_limit_is_rejected(
        self, store: FileSessionStore
    ) -> None:
        """加后缀后超长 → 拒绝（不截断 stem：那会把用户的标识符悄悄改样）。

        文案点明是**冲突后缀**把名字顶出上限（原名本身合法，是"重名 + 超长"这个
        组合没有合法解）——否则用户看到的是一个自己没发过的、带着 `-2` 的名字。
        """
        name = "a" * FILE_NAME_MAX_BYTES
        store.write_file(SESSION, name, b"first")

        with pytest.raises(InvalidInputError) as excinfo:
            store.write_file(SESSION, name, b"second")
        assert "too long to disambiguate" in str(excinfo.value)
        assert sorted(p.name for p in files_dir(store).iterdir()) == [name]

    def test_invalid_name_never_touches_the_filesystem(
        self, store: FileSessionStore
    ) -> None:
        with pytest.raises(InvalidInputError):
            store.write_file(SESSION, "../escape", b"x")
        assert not files_dir(store).exists()
        assert not (store.root / "escape").exists()

    def test_empty_body_is_allowed_at_this_layer(self, store: FileSessionStore) -> None:
        """字节闸门（非空）在 manager 层——store 只管"写进去"。"""
        ref = store.write_file(SESSION, "empty.txt", b"")
        assert ref.bytes == 0
        assert (files_dir(store) / "empty.txt").read_bytes() == b""

    def test_two_sessions_do_not_share_the_namespace(
        self, store: FileSessionStore
    ) -> None:
        store.write_file("store-files-a", "notes.txt", b"a")
        ref = store.write_file("store-files-b", "notes.txt", b"b")
        assert ref.name == "notes.txt"
        assert (files_dir(store, "store-files-b") / "notes.txt").read_bytes() == b"b"

    def test_path_is_absolute_even_with_a_relative_root(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """相对 root（`WING_SESSIONS_PATH=sessions`）下返回的 path 仍是绝对路径。

        store 的 I/O 本来就相对进程 cwd，而消费方要把 path 写进消息正文让 agent 去读
        ——相对路径到了 agent 侧会按它自己的 cwd 解析（读错文件）。
        """
        monkeypatch.chdir(tmp_path)
        relative = FileSessionStore("sessions")

        ref = relative.write_file(SESSION, "notes.txt", b"hello")

        assert Path(str(ref.path)).is_absolute()
        assert ref.path == str(tmp_path / "sessions" / SESSION / "files" / "notes.txt")
        assert Path(str(ref.path)).read_bytes() == b"hello"


# ── 3. memory 后端 ─────────────────────────────────────────────


class TestMemoryStoreWriteFile:
    def test_same_naming_semantics_without_a_path(self) -> None:
        store = MemorySessionStore()
        first = store.write_file("mem-1", "notes.txt", b"one")
        second = store.write_file("mem-1", "notes.txt", b"one")
        third = store.write_file("mem-1", "notes.txt", b"two")

        assert first == FileRef(name="notes.txt", path=None, bytes=3)
        assert second == first  # 同字节幂等
        assert third == FileRef(name="notes-2.txt", path=None, bytes=3)

    def test_buckets_are_per_session(self) -> None:
        store = MemorySessionStore()
        store.write_file("mem-1", "notes.txt", b"one")
        other = store.write_file("mem-2", "notes.txt", b"two")
        assert other.name == "notes.txt"  # 另一个会话的桶里没有冲突

    def test_invalid_name_is_rejected(self) -> None:
        store = MemorySessionStore()
        with pytest.raises(InvalidInputError):
            store.write_file("mem-1", "a/b", b"x")

    def test_suffix_overflow_leaves_no_bucket_entry(self) -> None:
        """拒绝路径不留下空桶条目（"失败零副作用"在内存后端也成立）。"""
        store = MemorySessionStore()
        name = "a" * FILE_NAME_MAX_BYTES
        store.write_file("mem-1", name, b"first")

        with pytest.raises(InvalidInputError) as excinfo:
            store.write_file("mem-1", name, b"second")

        assert "too long to disambiguate" in str(excinfo.value)
        assert sorted(store._files["mem-1"]) == [name]
