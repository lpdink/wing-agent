"""文件类工具的错误文案门禁（Read / ReadImage / Edit / Write）。

这四条文案是**模型可见输出**（tool result 回灌给模型，影响它下一步怎么做），
必须逐字稳定。`tools/internal/io_errors.reason_of` 是原因串的唯一实现，本文件把它与
四个工具各自的模板一起钉住：任何一处改动都会在这里红，而不是等用户发现模型开始瞎猜。

覆盖三条固定原因（``No such file`` / ``Permission denied`` / ``Is a directory``）
与 `write` 的两套模板（``cannot write`` / ``error writing``）。

与既有测试的分工：`test_read_tool` / `test_read_image` / `test_edit_tool` 里那批
``pytest.raises(match=...)`` 只断子串，本文件断**整串相等**——前者覆盖语义、
后者防文案被无声改写。
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from wing.schema import ToolError
from wing.tools.builtin.edit import edit_file
from wing.tools.builtin.read import read_file

# ReadImage 有视觉能力门禁（能力声明在最前，绝不触碰文件系统）：这里只钉文案，
# 不需要真图片——缺文件 / 权限 / 目录三种失败都发生在能力门禁之后。
from wing.tools.builtin.read_image import read_image
from wing.tools.builtin.write import write_file
from wing.tools.internal.io_errors import reason_of


class _Vision:
    vision = True


class _Ctx:
    """read_image 只需要 capabilities.vision（门禁）与 media（本轮用不到）。"""

    capabilities = _Vision()
    media = None
    cwd = None


@pytest.mark.parametrize(
    ("exc", "reason"),
    [
        (FileNotFoundError(2, "No such file or directory"), "No such file"),
        (PermissionError(13, "Permission denied"), "Permission denied"),
        (IsADirectoryError(21, "Is a directory"), "Is a directory"),
        (
            OSError(40, "Too many levels of symbolic links"),
            "Too many levels of symbolic links",
        ),
        (OSError(5, ""), "[Errno 5] "),
    ],
)
def test_reason_of(exc: OSError, reason: str):
    assert reason_of(exc) == reason


@pytest.mark.asyncio
class TestReadToolTexts:
    async def test_missing_file(self, tmp_path: Path):
        with pytest.raises(ToolError) as err:
            await read_file(str(tmp_path / "nope.txt"), ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"read_file: {tmp_path / 'nope.txt'}: No such file"

    async def test_permission_denied(self, tmp_path: Path, monkeypatch):
        target = tmp_path / "a.txt"
        target.write_text("x")
        real_stat = os.stat

        def _denied(path, *args, **kwargs):
            if str(path) == str(target):
                raise PermissionError(13, "Permission denied")
            return real_stat(path, *args, **kwargs)

        monkeypatch.setattr("wing.tools.builtin.read.os.stat", _denied)
        with pytest.raises(ToolError) as err:
            await read_file(str(target), ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"read_file: {target}: Permission denied"

    async def test_directory(self, tmp_path: Path):
        with pytest.raises(ToolError) as err:
            await read_file(str(tmp_path), ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"read_file: {tmp_path}: Is a directory"

    async def test_open_failure_keeps_exception_text(self, tmp_path: Path, monkeypatch):
        """open 段的既有错误面：``{e}``（含 errno 前缀），与其他段不同。"""
        target = tmp_path / "a.txt"
        target.write_text("x")

        def _boom(*args, **kwargs):
            raise OSError(5, "Input/output error")

        monkeypatch.setattr("builtins.open", _boom)
        with pytest.raises(ToolError) as err:
            await read_file(str(target), ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == (f"read_file: {target}: [Errno 5] Input/output error")


@pytest.mark.asyncio
class TestReadImageTexts:
    async def test_missing_file(self, tmp_path: Path):
        with pytest.raises(ToolError) as err:
            await read_image(str(tmp_path / "nope.png"), ctx=_Ctx())  # type: ignore[arg-type]
        assert str(err.value) == f"ReadImage: {tmp_path / 'nope.png'}: No such file"

    async def test_permission_denied(self, tmp_path: Path, monkeypatch):
        target = tmp_path / "a.png"
        target.write_bytes(b"x")
        real_stat = os.stat

        def _denied(path, *args, **kwargs):
            if str(path) == str(target):
                raise PermissionError(13, "Permission denied")
            return real_stat(path, *args, **kwargs)

        monkeypatch.setattr("wing.tools.builtin.read_image.os.stat", _denied)
        with pytest.raises(ToolError) as err:
            await read_image(str(target), ctx=_Ctx())  # type: ignore[arg-type]
        assert str(err.value) == f"ReadImage: {target}: Permission denied"

    async def test_other_oserror_reports_strerror(self, tmp_path: Path, monkeypatch):
        target = tmp_path / "a.png"
        target.write_bytes(b"x")
        real_stat = os.stat

        def _broken(path, *args, **kwargs):
            if str(path) == str(target):
                raise OSError(5, "Input/output error")
            return real_stat(path, *args, **kwargs)

        monkeypatch.setattr("wing.tools.builtin.read_image.os.stat", _broken)
        with pytest.raises(ToolError) as err:
            await read_image(str(target), ctx=_Ctx())  # type: ignore[arg-type]
        assert str(err.value) == f"ReadImage: {target}: Input/output error"

    async def test_directory(self, tmp_path: Path):
        with pytest.raises(ToolError) as err:
            await read_image(str(tmp_path), ctx=_Ctx())  # type: ignore[arg-type]
        assert str(err.value) == f"ReadImage: {tmp_path}: Is a directory"

    async def test_open_failure(self, tmp_path: Path, monkeypatch):
        target = tmp_path / "a.png"
        target.write_bytes(b"x")

        real_open = open

        def _denied(path, *args, **kwargs):
            if str(path) == str(target):
                raise PermissionError(13, "Permission denied")
            return real_open(path, *args, **kwargs)

        monkeypatch.setattr("builtins.open", _denied)
        with pytest.raises(ToolError) as err:
            await read_image(str(target), ctx=_Ctx())  # type: ignore[arg-type]
        assert str(err.value) == f"ReadImage: {target}: Permission denied"


@pytest.mark.asyncio
class TestEditToolTexts:
    async def test_missing_file(self, tmp_path: Path):
        target = tmp_path / "nope.txt"
        with pytest.raises(ToolError) as err:
            await edit_file(str(target), "a", "b", ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"edit: {target}: No such file"

    async def test_permission_denied(self, tmp_path: Path, monkeypatch):
        target = tmp_path / "a.txt"
        target.write_text("a")

        def _denied(*args, **kwargs):
            raise PermissionError(13, "Permission denied")

        monkeypatch.setattr("builtins.open", _denied)
        with pytest.raises(ToolError) as err:
            await edit_file(str(target), "a", "b", ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"edit: {target}: Permission denied"

    async def test_directory(self, tmp_path: Path, monkeypatch):
        def _isadir(*args, **kwargs):
            raise IsADirectoryError(21, "Is a directory")

        monkeypatch.setattr("builtins.open", _isadir)
        with pytest.raises(ToolError) as err:
            await edit_file(str(tmp_path), "a", "b", ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"edit: {tmp_path}: Is a directory"


@pytest.mark.asyncio
class TestWriteToolTexts:
    """``Write`` 走 ``atomic_write_text``（tmp + replace），所以错误从那里打桩。"""

    @staticmethod
    def _fail(monkeypatch, exc: BaseException) -> None:
        def _boom(*args, **kwargs):
            raise exc

        monkeypatch.setattr("wing.tools.builtin.write.atomic_write_text", _boom)

    async def test_permission_denied(self, tmp_path: Path, monkeypatch):
        target = tmp_path / "a.txt"
        self._fail(monkeypatch, PermissionError(13, "Permission denied"))
        with pytest.raises(ToolError) as err:
            await write_file(str(target), "x", ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"write: cannot write '{target}': Permission denied"

    async def test_is_a_directory(self, tmp_path: Path, monkeypatch):
        self._fail(monkeypatch, IsADirectoryError(21, "Is a directory"))
        with pytest.raises(ToolError) as err:
            await write_file(str(tmp_path), "x", ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"write: cannot write '{tmp_path}': Is a directory"

    async def test_other_oserror_uses_strerror(self, tmp_path: Path, monkeypatch):
        """``write`` 的 OSError 分支**不**回落 ``str(e)``（与 read / read_image 不同）。"""
        target = tmp_path / "a.txt"
        self._fail(monkeypatch, OSError(28, "No space left on device"))
        with pytest.raises(ToolError) as err:
            await write_file(str(target), "x", ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == (
            f"write: cannot write '{target}': No space left on device"
        )

    async def test_non_oserror_uses_error_writing(self, tmp_path: Path, monkeypatch):
        target = tmp_path / "a.txt"
        self._fail(monkeypatch, ValueError("bad"))
        with pytest.raises(ToolError) as err:
            await write_file(str(target), "x", ctx=None)  # type: ignore[arg-type]
        assert str(err.value) == f"write: error writing '{target}': bad"
