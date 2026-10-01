# wing/tools/read_image.py
"""ReadImage — 把图片文件读入会话，作为媒体附件让模型"看见"。

链路：文件字节 →（magic bytes 判定 + 头部尺寸解析）→ SessionStore 内容寻址
入库 → 返回 ``ToolOutput``（模型可见的文本信封 + 媒体引用）。字节从不进
history、从不进工具文本——模型在请求期按引用"看到"图片，前端只渲染信封。

两条硬约束（由单测锁死）：
- **视觉能力门禁在任何文件 I/O 之前**：模型未声明 ``capabilities: {vision: true}``
  时直接拒绝，不 stat、不读字节、不写媒体——"拒绝不产生副作用"；
- **失败一律 ``ToolError``**：错误文案回灌给模型（tool_success=false），面向
  "下一步怎么做"（换模型 / 转换格式 / 降采样），不是异常逃逸。
"""

from __future__ import annotations

import json
import os
import shlex
import stat as stat_module
from pathlib import Path

from wing.agent import ToolContext
from wing.config import ProviderConfig, get_config
from wing.media import (
    SUPPORTED_IMAGE_MIMES,
    format_image_envelope,
    format_size,
    image_dimensions,
    is_apple_cgbi_png,
    media_id,
    sniff_image_mime,
)
from wing.schema import MediaRef, ToolError, ToolOutput
from wing.tool_registry import tool_registry
from wing.tools.utils import resolve_path as _resolve_path

# 降采样建议的目标长边（Anthropic 视觉推荐上限 1568px；Retina 截图常见
# 3024px 宽，缩到一半即达标）。macOS 自带 sips，不需要额外依赖。
_DOWNSCALE_MAX_EDGE = 1568


@tool_registry.register(name="ReadImage")
async def read_image(path: str, ctx: ToolContext) -> ToolOutput:
    """Read an image file and attach it to the conversation so you can see it.

    Supports PNG, JPEG, WebP and GIF (detected by file content, not by the
    file extension). The image bytes are stored in the session media store;
    this tool returns a one-line text envelope instead of the bytes.

    Args:
        path: Image file path (relative paths resolve against the workspace).

    Returns:
        A single-line envelope `[image: PATH | FORMAT WxH | SIZE | id ID | mtime MTIME]`
        with the image attached.
    """
    resolved = _resolve_path(path, ctx)

    # 门禁在最前：无视觉能力 → 直接拒绝，绝不触碰文件系统（拒绝不产生副作用）。
    if not ctx.capabilities.vision:
        raise ToolError(_no_vision_message(ctx))

    try:
        st = os.stat(resolved)
    except FileNotFoundError:
        raise ToolError(f"ReadImage: {resolved}: No such file")
    except PermissionError:
        raise ToolError(f"ReadImage: {resolved}: Permission denied")
    except OSError as e:
        raise ToolError(f"ReadImage: {resolved}: {e.strerror or e}")

    if stat_module.S_ISDIR(st.st_mode):
        raise ToolError(f"ReadImage: {resolved}: Is a directory")
    if st.st_size == 0:
        raise ToolError(f"ReadImage: {resolved}: empty file (0 bytes)")

    max_bytes = get_config().images.max_bytes
    if st.st_size > max_bytes:
        raise _too_large_error(resolved, st.st_size, max_bytes)

    # 字节只读一次：格式判定 / 尺寸解析 / 内容哈希（入库 id）都消费同一份 data。
    try:
        with open(resolved, "rb") as f:
            data = f.read()
    except FileNotFoundError:
        raise ToolError(f"ReadImage: {resolved}: No such file")
    except PermissionError:
        raise ToolError(f"ReadImage: {resolved}: Permission denied")
    except OSError as e:
        raise ToolError(f"ReadImage: {resolved}: {e.strerror or e}")

    # stat 与 read 之间存在窗口（文件被追加写）：以后读到的字节为准再判一次。
    if len(data) > max_bytes:
        raise _too_large_error(resolved, len(data), max_bytes)

    mime = sniff_image_mime(data)
    if mime is None:
        supported = "/".join(sorted(_short_mime(m) for m in SUPPORTED_IMAGE_MIMES))
        raise ToolError(
            f"ReadImage: {resolved}: not a supported image ({supported} expected; "
            f"detected from file content, not from the extension). "
            f"Convert it first, e.g.: "
            f"sips -s format png {shlex.quote(resolved)} "
            f"--out {shlex.quote(resolved + '.png')}"
        )

    dims = image_dimensions(data, mime)
    if dims is None:
        if is_apple_cgbi_png(data):
            # CgBI 是 Apple 的非标准 PNG 变体（IHDR 在偏移 28）：文件本身合法
            # 可显示，只是尺寸无法按标准布局解析——归因必须准确（"损坏"是错的），
            # 并给出可照抄的转换命令（sips 能读 CgBI）。
            raise ToolError(
                f"ReadImage: {resolved}: Apple CgBI PNG variant — dimensions "
                f"cannot be read by this tool (non-standard layout: the CgBI "
                f"chunk precedes IHDR). Convert it first, e.g.: "
                f"sips -s format png {shlex.quote(resolved)} "
                f"--out {shlex.quote(resolved + '.png')}"
            )
        raise ToolError(
            f"ReadImage: {resolved}: image header is truncated or corrupt "
            f"(cannot parse {_short_mime(mime)} dimensions)"
        )
    width, height = dims

    media = ctx.media
    if media is None:
        raise ToolError(
            "ReadImage: no media store is mounted for this session — "
            "images cannot be attached"
        )

    ref = MediaRef(
        id=media_id(data),
        mime=mime,
        bytes=len(data),
        width=width,
        height=height,
        name=Path(resolved).name,
    )
    try:
        media.write(ref.id, data)
    except OSError as e:
        # 入库失败意味着引用无法兑现（后续请求只会看到占位文本）——
        # 明确失败，别给模型一个"看起来成功"的信封。
        raise ToolError(
            f"ReadImage: {resolved}: failed to persist image bytes: {e.strerror or e}"
        )

    content = format_image_envelope(resolved, ref, int(st.st_mtime))
    return ToolOutput(content=content, media=[ref])


def _short_mime(mime: str) -> str:
    """image/png → png（文案用短名）。"""
    return mime.removeprefix("image/")


def _too_large_error(resolved: str, nbytes: int, max_bytes: int) -> ToolError:
    """单图超限的错误：实际大小 / 上限 / 降采样示例（可复制执行）。

    示例命令对路径做 shell 引用（``shlex.quote``）——含空格 / CJK 的路径
    照抄即可执行（review r1 S1）。
    """
    return ToolError(
        f"ReadImage: {resolved}: {format_size(nbytes)} exceeds the "
        f"{format_size(max_bytes)} per-image limit (config images.max_bytes). "
        f"Downscale it first, then read the smaller file, e.g.: "
        f"sips -Z {_DOWNSCALE_MAX_EDGE} {shlex.quote(resolved)} "
        f"--out {shlex.quote(resolved + '.small.png')}"
    )


_UNKNOWN_PROVIDER = "<your-provider>"
"""示例里无法指认 provider 归属时的中性占位。

带尖括号、不是合法 provider 名（``^[a-zA-Z0-9_-]+$``）——不会被误读成真实
配置值；文案的 Note 句说明它代表"本会话实际使用的那个 provider"。
"""


def _no_vision_message(ctx: ToolContext) -> str:
    """门禁文案：模型名 + 未读文件 + 开启方法 +（可选）同 provider 的 vision 模型。

    工具只持有窄接口 ToolContext（没有 provider 句柄），归属靠扫描
    ``get_config().providers`` 得到：
    - 恰好一个 provider 声明了该模型 → 写它的名字，并可在其中点名一个声明了
      vision 的兄弟模型；
    - 未声明 / 多个 provider 同名声明 → **不指认归属**：同名模型可能挂在
      非活跃 provider 上（前端 picker 明确支持同名模型跨 provider 区分），
      点名一个错的配置位置会把模型引向无效操作；此时也用中性占位替代
      provider 名，并同样省略"切换模型"建议（同样的归属不确定问题）。
    """
    model = ctx.model
    matches = _matching_providers(model)
    unique = matches[0] if len(matches) == 1 else None
    lines = [
        f"ReadImage: model '{model}' does not declare vision capability — "
        f"images cannot be attached. No file was read.",
        f"To enable it, declare vision for '{model}' in config.yaml, e.g.:",
        "  providers:",
        f"    - name: {unique.name if unique is not None else _UNKNOWN_PROVIDER}",
        "      models:",
        # model 名可能含 YAML 元字符（`:` / `#` / 前导 `*` 等）——裸插值会产
        # 出不可解析的片段；json.dumps 的双引号标量是合法 YAML（转义由它保证）。
        f"        - name: {json.dumps(model)}",
        "          capabilities: {vision: true}",
    ]
    if unique is not None:
        sibling = _vision_sibling(unique, model)
        if sibling is not None:
            lines.append(
                f"Alternatively, switch to a model that declares vision "
                f"(e.g. '{sibling}' in provider '{unique.name}')."
            )
    else:
        reason = (
            f"'{model}' is declared by several providers"
            if matches
            else f"'{model}' is not declared by any provider in the current config"
        )
        lines.append(
            f"Note: {reason} — {_UNKNOWN_PROVIDER} above stands for the "
            f"provider this session actually uses."
        )
    return "\n".join(lines)


def _matching_providers(model: str) -> list[ProviderConfig]:
    """配置里声明了该模型名的全部 provider（0 / 1 / N 个）。"""
    return [p for p in get_config().providers if p.find_model(model) is not None]


def _vision_sibling(provider: ProviderConfig, model: str) -> str | None:
    """同 provider 内另一个声明了 vision 的模型名；没有则 None。"""
    for spec in provider.models:
        if isinstance(spec, str):
            continue
        if spec.name != model and spec.capabilities.vision:
            return spec.name
    return None
