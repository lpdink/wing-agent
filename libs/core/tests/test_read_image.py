# libs/core/tests/test_read_image.py
"""ReadImage 工具单测：门禁顺序 / 信封 / 校验 / 存量指引 / 能力跟随切换。

最重要的回归：**视觉门禁在任何文件 I/O 之前**——vision=false 时 os.stat /
open / media.write 一次都不能发生（用 spy 锁死，"拒绝不产生副作用"）。
"""

from __future__ import annotations

import builtins
import hashlib
import io
import os
import shlex
import stat as stat_module
import struct
from pathlib import Path
from types import SimpleNamespace
from typing import cast

import pytest
import pytest_asyncio
import yaml

from wing.agent import ToolContext
from wing.config import ModelCapabilities, ModelSpec
from wing.media import MediaAccess
from wing.schema import ToolError, ToolOutput
from wing.tool_registry import tool_registry
from wing.tools.builtin.read import read_file
from wing.tools.builtin.read_image import read_image


def png_bytes(width: int, height: int) -> bytes:
    """最小 PNG 头部（sig + IHDR）——ReadImage 只做头部解析，不解码像素。"""
    return (
        b"\x89PNG\r\n\x1a\n"
        + struct.pack(">I", 13)
        + b"IHDR"
        + struct.pack(">II", width, height)
        + b"\x08\x06\x00\x00\x00"
    )


def jpeg_bytes(width: int, height: int) -> bytes:
    """最小 JPEG（SOI + SOF0 段）。"""
    payload = (
        b"\x08"
        + struct.pack(">HH", height, width)
        + b"\x03\x01\x11\x00\x02\x11\x01\x03\x11\x01"
    )
    return b"\xff\xd8" + b"\xff\xc0" + struct.pack(">H", len(payload) + 2) + payload


def write_image(path: Path, data: bytes, *, total_size: int | None = None) -> bytes:
    """落盘图片（可选填充到指定总字节数——测上限边界用）。"""
    if total_size is not None:
        assert total_size >= len(data)
        data = data + b"\x00" * (total_size - len(data))
    path.write_bytes(data)
    return data


class _SpyMedia:
    """MediaAccess spy：记录读写调用的 id / 字节。"""

    def __init__(self) -> None:
        self.reads: list[str] = []
        self.writes: list[tuple[str, bytes]] = []

    def read(self, media_id: str) -> bytes | None:
        self.reads.append(media_id)
        return None

    def write(self, media_id: str, data: bytes) -> None:
        self.writes.append((media_id, data))

    @property
    def access(self) -> MediaAccess:
        return MediaAccess(read=self.read, write=self.write)


class _StubCtx:
    """最小 ToolContext stub（只实现 ReadImage 消费的窄接口）。"""

    def __init__(
        self,
        *,
        vision: bool,
        model: str = "text-model",
        media: MediaAccess | None = None,
    ) -> None:
        self._capabilities = ModelCapabilities(vision=vision)
        self.model = model
        self.media = media

    @property
    def capabilities(self) -> ModelCapabilities:
        return self._capabilities


def _ctx(
    *,
    vision: bool,
    model: str = "text-model",
    media: MediaAccess | None = None,
) -> ToolContext:
    return cast(ToolContext, _StubCtx(vision=vision, model=model, media=media))


def _example_yaml_snippet(msg: str) -> str:
    """从门禁文案里取出可照抄的 config.yaml 片段（剥掉文案的两空格缩进）。"""
    lines = msg.splitlines()
    start = lines.index("  providers:")
    end = next(
        index
        for index in range(start, len(lines))
        if lines[index].strip().startswith("capabilities:")
    )
    return "\n".join(line[2:] for line in lines[start : end + 1])


########## 1. 视觉能力门禁（在 I/O 之前）


class TestVisionGate:
    @pytest.mark.asyncio
    async def test_refuses_before_any_file_io(self, tmp_path: Path):
        """vision=false → ToolError 且零副作用：所有已知 I/O 入口都被禁止。"""
        image = tmp_path / "shot.png"
        write_image(image, png_bytes(800, 600))
        spy = _SpyMedia()

        def _forbidden(*_args, **_kwargs):
            raise AssertionError("file I/O happened before the vision gate")

        # 覆盖面 = 实现可能用到的每一个 I/O 入口（现实现只用 os.stat + open）。
        # 未来若重构成 Path.read_bytes / io.open，这组 spy 同样会拦截。
        forbidden_io = (
            (os, "stat"),
            (os, "lstat"),
            (os, "open"),
            (os, "listdir"),
            (os, "scandir"),
            (os.path, "exists"),
            (os.path, "getsize"),
            (builtins, "open"),
            (io, "open"),
            (Path, "open"),
            (Path, "stat"),
            (Path, "read_bytes"),
            (Path, "read_text"),
            (Path, "exists"),
            (Path, "is_file"),
            (Path, "is_dir"),
        )

        with pytest.MonkeyPatch.context() as mp:
            for target, name in forbidden_io:
                mp.setattr(target, name, _forbidden)
            with pytest.raises(ToolError) as ei:
                await read_image(str(image), ctx=_ctx(vision=False, media=spy.access))

        assert spy.writes == []
        assert spy.reads == []

        msg = str(ei.value)
        # 模型名 + 明确"未读文件" + 可直接照抄的配置示例
        assert "text-model" in msg
        assert "No file was read." in msg
        assert "capabilities: {vision: true}" in msg

    @pytest.mark.asyncio
    async def test_message_suggests_vision_sibling_in_same_provider(
        self, tmp_path: Path, _mock_config
    ):
        """恰好一个 provider 声明该模型：写它的名字，点名 vision 兄弟；没有兄弟则省略建议。"""
        _mock_config.providers[0].models = [
            ModelSpec(name="text-model"),
            ModelSpec(name="vision-model", capabilities=ModelCapabilities(vision=True)),
        ]
        image = tmp_path / "shot.png"
        write_image(image, png_bytes(4, 4))

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=False, model="text-model"))
        msg = str(ei.value)
        assert "vision-model" in msg
        assert "- name: default" in msg  # provider 名（配置里的 providers[0].name）
        assert "<your-provider>" not in msg
        assert "Note:" not in msg

        # 没有 vision 兄弟 → 不出现建议句
        _mock_config.providers[0].models = [ModelSpec(name="text-model")]
        with pytest.raises(ToolError) as ei2:
            await read_image(str(image), ctx=_ctx(vision=False, model="text-model"))
        assert "Alternatively" not in str(ei2.value)

    @pytest.mark.asyncio
    async def test_multi_provider_same_name_omits_attribution(
        self, tmp_path: Path, _mock_config
    ):
        """同名模型挂在多个 provider：不指认归属（无法判定活跃者），示例用中性占位。"""
        _mock_config.providers[0].models = [ModelSpec(name="shared-model")]
        _mock_config.providers[1].models = [
            ModelSpec(name="shared-model"),
            ModelSpec(name="alt-vision", capabilities=ModelCapabilities(vision=True)),
        ]
        image = tmp_path / "shot.png"
        write_image(image, png_bytes(4, 4))

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=False, model="shared-model"))
        msg = str(ei.value)
        assert "- name: <your-provider>" in msg
        assert "declared by several providers" in msg
        assert "the provider this session actually uses" in msg
        # 不能把模型引向一个可能非活跃的 provider / 其 vision 兄弟
        assert "- name: default" not in msg
        assert "- name: alt" not in msg
        assert "alt-vision" not in msg
        assert "Alternatively" not in msg

    @pytest.mark.asyncio
    async def test_gate_is_reachable_without_config_provider_match(
        self, tmp_path, _mock_config
    ):
        """模型不在任何 provider 声明里（如 CLI override）→ 中性占位 + Note，仍拒绝。"""
        _mock_config.providers[0].models = []
        _mock_config.providers[1].models = []
        image = tmp_path / "shot.png"
        write_image(image, png_bytes(4, 4))

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=False, model="ghost-model"))
        msg = str(ei.value)
        assert "ghost-model" in msg
        assert '- name: "ghost-model"' in msg  # 示例用同一模型名（YAML 引用形态）
        # 占位不是合法 provider 名（不会被照抄成真实配置），并有 Note 说明
        assert "- name: <your-provider>" in msg
        assert "not declared by any provider" in msg
        assert "the provider this session actually uses" in msg
        assert "Alternatively" not in msg

    @pytest.mark.asyncio
    async def test_example_yaml_survives_metachar_model_names(
        self, tmp_path: Path, _mock_config
    ):
        """模型名含 YAML 元字符（`:` / `#` / 前导 `*`）时示例片段仍可解析（N2）。

        裸插值会让 `- name: openai:gpt-4o` 这类行直接语法错误（或把 `#` 后
        截断），模型照抄即拿到配置错误——引用形态必须让它逐字可解析。
        """
        image = tmp_path / "shot.png"
        write_image(image, png_bytes(4, 4))

        for model in ("openai:gpt-4o", "a #b", "*star"):
            with pytest.raises(ToolError) as ei:
                await read_image(str(image), ctx=_ctx(vision=False, model=model))
            snippet = _example_yaml_snippet(str(ei.value))
            assert yaml.safe_load(snippet) == {
                "providers": [
                    {
                        "name": "<your-provider>",
                        "models": [{"name": model, "capabilities": {"vision": True}}],
                    }
                ]
            }


########## 2. 成功路径


class TestSuccess:
    @pytest.mark.asyncio
    async def test_returns_envelope_and_stores_bytes(self, tmp_path: Path):
        image = tmp_path / "shot.png"
        data = write_image(image, png_bytes(800, 600))
        spy = _SpyMedia()

        out = await read_image(str(image), ctx=_ctx(vision=True, media=spy.access))

        assert isinstance(out, ToolOutput)
        assert len(out.media) == 1
        ref = out.media[0]
        assert ref.id == hashlib.sha256(data).hexdigest()
        assert (ref.mime, ref.bytes, ref.width, ref.height) == (
            "image/png",
            len(data),
            800,
            600,
        )
        assert ref.name == "shot.png"

        # 字节经 media.write 入库（内容寻址 id）
        assert spy.writes == [(ref.id, data)]

        # 信封：单行、无 base64
        line = out.content
        assert line.startswith(f"[image: {image} | ")
        assert "| png 800x600 |" in line
        assert f"| id {ref.id[:8]} |" in line
        assert "| mtime " in line
        assert "\n" not in line
        assert "base64" not in line

    @pytest.mark.asyncio
    async def test_detects_format_from_bytes_not_extension(self, tmp_path: Path):
        """扩展名撒谎 → 以 bytes 为准（.png 结尾的 JPEG 按 jpeg 处理）。"""
        image = tmp_path / "lies.png"
        write_image(image, jpeg_bytes(100, 50))
        spy = _SpyMedia()

        out = await read_image(str(image), ctx=_ctx(vision=True, media=spy.access))

        assert out.media[0].mime == "image/jpeg"
        assert "| jpeg 100x50 |" in out.content
        assert out.media[0].name == "lies.png"

    @pytest.mark.asyncio
    async def test_relative_path_resolves_against_workspace(self, tmp_path: Path):
        image = tmp_path / "shot.png"
        data = write_image(image, png_bytes(2, 2))
        spy = _SpyMedia()

        class _CwdCtx(_StubCtx):
            @property
            def cwd(self) -> Path:
                return tmp_path

        out = await read_image(
            "shot.png",
            ctx=cast(
                ToolContext,
                _CwdCtx(vision=True, media=spy.access),
            ),
        )
        assert out.media[0].id == hashlib.sha256(data).hexdigest()
        assert str(tmp_path / "shot.png") in out.content


########## 3. 单图上限


class TestMaxBytes:
    @pytest.mark.asyncio
    async def test_over_limit_rejected_with_downscale_hint(
        self, tmp_path: Path, _mock_config
    ):
        _mock_config.images.max_bytes = 128
        image = tmp_path / "big.png"
        write_image(image, png_bytes(800, 600), total_size=129)
        spy = _SpyMedia()

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=True, media=spy.access))

        msg = str(ei.value)
        assert "129 bytes" in msg
        assert "128 bytes" in msg
        assert "images.max_bytes" in msg
        assert "sips -Z 1568" in msg
        assert spy.writes == []

    @pytest.mark.asyncio
    async def test_exactly_at_limit_passes(self, tmp_path: Path, _mock_config):
        _mock_config.images.max_bytes = 128
        image = tmp_path / "edge.png"
        data = write_image(image, png_bytes(4, 4), total_size=128)
        spy = _SpyMedia()

        out = await read_image(str(image), ctx=_ctx(vision=True, media=spy.access))

        assert out.media[0].bytes == 128
        assert spy.writes[0][1] == data

    @pytest.mark.asyncio
    async def test_downscale_hint_shell_quotes_path(self, tmp_path: Path, _mock_config):
        """含空格 / CJK 的路径在示例命令里被 shell 引用，可直接复制执行（S1）。"""
        _mock_config.images.max_bytes = 64
        spaced = tmp_path / "工作 目录"
        spaced.mkdir()
        image = spaced / "截图 shot.png"
        write_image(image, png_bytes(4, 4), total_size=128)

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=True))

        msg = str(ei.value)
        # 按 shell 词法拆出的 argv 必须恰好是 6 个 token（未引用会被拆成 10 个）
        argv = shlex.split(msg.split("e.g.:", 1)[1].strip())
        assert argv == [
            "sips",
            "-Z",
            "1568",
            str(image),
            "--out",
            str(image) + ".small.png",
        ]

    @pytest.mark.asyncio
    async def test_growth_between_stat_and_read_still_rejected(
        self, tmp_path: Path, monkeypatch, _mock_config
    ):
        """stat 与 read 之间的追加写窗口：以实际读到的字节复判上限。"""
        _mock_config.images.max_bytes = 64
        image = tmp_path / "grow.png"
        write_image(image, png_bytes(4, 4), total_size=128)
        # 伪造一个"很小"的 stat 结果（st_size=32），模拟 stat 之后文件被追加写
        fake = SimpleNamespace(
            st_mode=stat_module.S_IFREG | 0o644, st_size=32, st_mtime=1.0
        )
        monkeypatch.setattr(os, "stat", lambda *_a, **_k: fake)

        with pytest.raises(ToolError, match="exceeds"):
            await read_image(str(image), ctx=_ctx(vision=True))


########## 4. 默认上限（4.5 MiB）的边界行为


class TestDefaultMaxBytes:
    """默认 `images.max_bytes` = 4_718_592 的边界：= 上限可读 / +1 拒绝。

    `_mock_config` 不覆盖 `images`，所以这里跑的就是解析出的默认值——两个用例
    合并把默认值**行为上**钉死：恰好 4_718_592（放宽 → "+1 拒绝"变红；收紧 →
    "= 上限可读"变红）。fixture 字节数与默认值同源，改动必须一起改。
    """

    #: 默认上限的字节数（4.5 MiB）——与 config.py / default_config.py 同值。
    DEFAULT_MAX_BYTES = 4_718_592

    @pytest.mark.asyncio
    async def test_exactly_at_default_limit_passes(self, tmp_path: Path, _mock_config):
        assert _mock_config.images.max_bytes == self.DEFAULT_MAX_BYTES, (
            "默认上限变了——fixture 字节数必须同步"
        )
        image = tmp_path / "at-limit.png"
        data = write_image(image, png_bytes(4, 4), total_size=self.DEFAULT_MAX_BYTES)
        spy = _SpyMedia()

        out = await read_image(str(image), ctx=_ctx(vision=True, media=spy.access))

        assert out.media[0].bytes == self.DEFAULT_MAX_BYTES
        assert spy.writes[0][1] == data

    @pytest.mark.asyncio
    async def test_one_byte_over_default_limit_rejected(
        self, tmp_path: Path, _mock_config
    ):
        assert _mock_config.images.max_bytes == self.DEFAULT_MAX_BYTES, (
            "默认上限变了——fixture 字节数必须同步"
        )
        image = tmp_path / "over-limit.png"
        write_image(image, png_bytes(4, 4), total_size=self.DEFAULT_MAX_BYTES + 1)
        spy = _SpyMedia()

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=True, media=spy.access))

        msg = str(ei.value)
        # 上限字符串是 format_size(4_718_592) == "4.5 MB"（锚定模型可见文案形态）
        assert "the 4.5 MB per-image limit" in msg, msg
        assert "images.max_bytes" in msg, msg
        # 拒绝发生在读字节 / 入库之前（"拒绝不产生副作用"）
        assert spy.reads == []
        assert spy.writes == []


########## 5. 文件 / 格式校验


class TestFileValidation:
    @pytest.mark.asyncio
    async def test_unsupported_format(self, tmp_path: Path):
        image = tmp_path / "notes.bin"
        image.write_bytes(b"\x00\x01\x02\x03" * 8)

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=True))
        msg = str(ei.value)
        assert "not a supported image" in msg
        assert "sips -s format png" in msg

    @pytest.mark.asyncio
    async def test_conversion_hint_shell_quotes_path(self, tmp_path: Path):
        """格式转换示例命令同样对路径做 shell 引用（S1）。"""
        spaced = tmp_path / "工作 目录"
        spaced.mkdir()
        image = spaced / "notes.bin"
        image.write_bytes(b"\x00\x01\x02\x03" * 8)

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=True))

        msg = str(ei.value)
        argv = shlex.split(msg.split("e.g.:", 1)[1].strip())
        assert argv == [
            "sips",
            "-s",
            "format",
            "png",
            str(image),
            "--out",
            str(image) + ".png",
        ]

    @pytest.mark.asyncio
    async def test_corrupt_header(self, tmp_path: Path):
        image = tmp_path / "trunc.png"
        image.write_bytes(b"\x89PNG\r\n\x1a\n" + b"\x00" * 8)  # 签名在，IHDR 不完整

        with pytest.raises(ToolError, match="truncated or corrupt"):
            await read_image(str(image), ctx=_ctx(vision=True))

    @pytest.mark.asyncio
    async def test_apple_cgbi_png_reports_variant_not_corrupt(self, tmp_path: Path):
        """Apple CgBI 变体：报"变体需转换"，绝不误报"损坏"（S1）。

        CgBI = 签名后紧跟 CgBI chunk（IHDR 在偏移 28）——文件合法可显示，
        只是标准布局解析器读不到尺寸。归因必须指向变体本身 + 转换命令。
        """
        image = tmp_path / "apple.png"
        image.write_bytes(
            b"\x89PNG\r\n\x1a\n"
            + struct.pack(">I", 4)
            + b"CgBI"
            + b"PROF"  # CgBI chunk payload
            + b"\x00\x00\x00\x00"  # chunk CRC
            + b"\x00" * 16
        )

        with pytest.raises(ToolError) as ei:
            await read_image(str(image), ctx=_ctx(vision=True))

        msg = str(ei.value)
        assert "CgBI" in msg
        assert "Apple" in msg
        assert "truncated or corrupt" not in msg
        # 转换示例必须可照抄（路径 shell 引用）
        argv = shlex.split(msg.split("e.g.:", 1)[1].strip())
        assert argv == [
            "sips",
            "-s",
            "format",
            "png",
            str(image),
            "--out",
            str(image) + ".png",
        ]

    @pytest.mark.asyncio
    async def test_empty_file(self, tmp_path: Path):
        image = tmp_path / "empty.png"
        image.write_bytes(b"")

        with pytest.raises(ToolError, match="empty file"):
            await read_image(str(image), ctx=_ctx(vision=True))

    @pytest.mark.asyncio
    async def test_directory(self, tmp_path: Path):
        with pytest.raises(ToolError, match="Is a directory"):
            await read_image(str(tmp_path), ctx=_ctx(vision=True))

    @pytest.mark.asyncio
    async def test_missing_file(self, tmp_path: Path):
        with pytest.raises(ToolError, match="No such file"):
            await read_image(str(tmp_path / "nope.png"), ctx=_ctx(vision=True))

    @pytest.mark.asyncio
    async def test_no_media_store_mounted(self, tmp_path: Path):
        image = tmp_path / "shot.png"
        write_image(image, png_bytes(4, 4))

        with pytest.raises(ToolError, match="no media store"):
            await read_image(str(image), ctx=_ctx(vision=True, media=None))


########## 6. 工具注册契约（名字 / 参数名冻结）


class TestRegistrationContract:
    def test_tool_name_and_param_are_frozen(self):
        tool = tool_registry.get_tool("ReadImage")
        assert tool is not None
        schema = tool.to_openai()["function"]
        assert schema["name"] == "ReadImage"
        assert list(schema["parameters"]["properties"]) == ["path"]
        assert schema["parameters"]["required"] == ["path"]
        # ctx 是注入参数，不出现在 LLM 可见 schema 里
        assert tool.inject_agent_param == "ctx"


########## 7. 工具描述（模型可见的 docstring）


class TestToolDescription:
    """描述 = 模型可见文本：保留成功路径 + 形态说明，不枚举失败路径。

    失败路径的完整信息由**拒绝发生时的错误文案**给出（`_no_vision_message` /
    `_too_large_error`），静态描述只留成功语义与可行动形态——有意取舍
    （见 03_media_cap/design.md D3），改动必须让本类变红。
    """

    @staticmethod
    def _description() -> str:
        tool = tool_registry.get_tool("ReadImage")
        assert tool is not None
        return tool.to_openai()["function"]["description"]

    def test_failure_paths_are_not_enumerated(self):
        description = self._description()
        for fragment in (
            "no vision capability",
            "vision",  # 「需 vision 前置」表述整体移除（拒绝文案自解释）
            "unsupported format",
            "too large",
            "corrupt",
            "CgBI",
            "refused",
        ):
            assert fragment not in description, (fragment, description)

    def test_success_semantics_are_kept(self):
        description = self._description()
        assert "attach it to the conversation" in description
        assert "PNG, JPEG, WebP and GIF" in description
        assert "detected by file content, not by the" in description
        assert "media store" in description
        assert "one-line" in description and "envelope" in description
        assert "[image: PATH | FORMAT WxH | SIZE | id ID | mtime MTIME]" in description
        # Args: path 语义（相对路径按 workspace 解析）仍在
        assert "Image file path (relative paths resolve against the workspace)" in (
            description
        )


########## 8. Read 的图片指引


class TestReadHint:
    @pytest.mark.asyncio
    async def test_png_points_to_read_image(self, tmp_path: Path):
        image = tmp_path / "shot.png"
        write_image(image, png_bytes(800, 600))

        with pytest.raises(ToolError) as ei:
            await read_file(str(image), ctx=None)  # type: ignore[arg-type]
        assert "Binary file (image/png)" in str(ei.value)
        assert "use ReadImage to view images" in str(ei.value)

    @pytest.mark.asyncio
    async def test_random_binary_has_no_hint(self, tmp_path: Path):
        blob = tmp_path / "blob.dat"
        blob.write_bytes(b"\x00\x01\x02\x03" * 4)

        with pytest.raises(ToolError) as ei:
            await read_file(str(blob), ctx=None)  # type: ignore[arg-type]
        assert "Binary file" in str(ei.value)
        assert "ReadImage" not in str(ei.value)


########## 9. WingAgent.capabilities 跟随模型切换


@pytest_asyncio.fixture
async def agent():
    """真实 runtime 装配的 agent（provider 来自 conftest 的 mock config）。"""
    from wing.runtime import WingRuntime

    session = WingRuntime().create_session()
    try:
        yield session.agent
    finally:
        await session.agent.shutdown()


class TestAgentCapabilities:
    @pytest.mark.asyncio
    async def test_follows_model_switch(self, agent, _mock_config):
        _mock_config.providers[0].models = [
            ModelSpec(name="text-model"),
            ModelSpec(name="vision-model", capabilities=ModelCapabilities(vision=True)),
        ]

        agent.set_model("vision-model", agent.model_provider)
        assert agent.capabilities.vision is True

        # 声明了但没开 vision → 安全默认 false
        agent.set_model("text-model", agent.model_provider)
        assert agent.capabilities.vision is False

        # 完全未声明 → false（不做名字启发式）
        agent.set_model("undeclared-model", agent.model_provider)
        assert agent.capabilities.vision is False

        # 切回来仍然是实时解析（无缓存）
        agent.set_model("vision-model", agent.model_provider)
        assert agent.capabilities.vision is True
