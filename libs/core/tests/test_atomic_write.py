"""原子写的并发回归（review r1 S1）。

背景：tmp 名曾只按 pid 唯一（`<name>.tmp.<pid>`）——同进程多线程并发写同一
路径时两个线程共用同一个 tmp，先到者 `os.replace` 改名后，后到者找不到源
文件抛 `FileNotFoundError`。修复后 tmp 名 = pid + 线程 id（见 `common/fs.py`
的 `_tmp_path`）。本文件用 Barrier 对齐的线程池把该竞态钉成回归测试。
"""

from __future__ import annotations

import hashlib
import threading
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from wing.common.fs import atomic_write_bytes, atomic_write_text
from wing.store import FileSessionStore


def _race(workers: int, fn: Callable[[], None]) -> list[Exception]:
    """Barrier 对齐后并发执行 fn，收集全部异常。"""
    barrier = threading.Barrier(workers)

    def run() -> None:
        barrier.wait()
        fn()

    errors: list[Exception] = []
    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = [pool.submit(run) for _ in range(workers)]
        for future in futures:
            try:
                future.result()
            except Exception as e:  # noqa: BLE001 - 收集所有失败供断言
                errors.append(e)
    return errors


class TestConcurrentAtomicWrite:
    def test_atomic_write_bytes_same_path(self, tmp_path: Path):
        target = tmp_path / "blob.bin"
        errors = _race(8, lambda: atomic_write_bytes(target, b"payload"))
        assert errors == []
        assert target.read_bytes() == b"payload"
        assert sorted(p.name for p in tmp_path.iterdir()) == ["blob.bin"]

    def test_atomic_write_text_same_path(self, tmp_path: Path):
        target = tmp_path / "metadata.json"
        errors = _race(8, lambda: atomic_write_text(target, "{}"))
        assert errors == []
        assert target.read_text(encoding="utf-8") == "{}"
        assert sorted(p.name for p in tmp_path.iterdir()) == ["metadata.json"]

    def test_store_write_media_same_id(self, tmp_path: Path):
        """真实调用路径：并发对同一文件写同一 media id（内容寻址 → 同内容）。"""
        store = FileSessionStore(tmp_path / "sessions")
        data = b"img-bytes"
        mid = hashlib.sha256(data).hexdigest()
        errors = _race(8, lambda: store.write_media(mid, data))
        assert errors == []
        assert store.read_media(mid) == data
        media_dir = tmp_path / "sessions" / ".media" / mid[:2]
        assert sorted(p.name for p in media_dir.iterdir()) == [mid]
