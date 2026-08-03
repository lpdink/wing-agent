"""SendFile 远程工具——Agent 把共享卷里的文件发到钉钉。

dingtalk 前端容器与工具宿主容器共享 workspace 卷（同挂载点），工具按
绝对路径读文件（限制在共享根内），经钉钉 media 上传 + OpenAPI 发送。
"""

from __future__ import annotations

import logging
import os
from pathlib import Path

from wing_sdk.host import ToolHost

from .config import Config
from .router import Router
from .sender import DingSender

log = logging.getLogger("wing-dingtalk.filetool")


def build_file_host(cfg: Config, router: Router, sender: DingSender) -> ToolHost:
    """构造注册了 SendFile 的 ToolHost（client_id = cfg.tool_client_id）。"""
    host = ToolHost(
        client_id=cfg.tool_client_id,
        gateway_url=cfg.gateway_url,
        api_key=cfg.api_key,
    )

    @host.tool(name="SendFile")
    async def send_file(path: str, comment: str = "") -> str:
        """Send a file from the shared workspace to the user via DingTalk.

        Use this to deliver generated artifacts (PDFs, images, archives,
        reports, ...) to the user. The file must live inside the shared
        workspace (the same filesystem the development tools see).

        Args:
            path: Absolute path of the file inside the workspace (e.g. /workspace/wing-agent/report.pdf).
            comment: Optional short message sent together with the file.
        """
        target = _resolve_inside_root(cfg.shared_root, path)
        media_id = await sender.upload_file(target)

        sent = 0
        errors: list[str] = []
        suffix = target.suffix.lstrip(".") or "file"
        convs = router.bound_conversations() or router.all_conversations()
        for conv in convs:
            try:
                if comment:
                    await sender.send_text(conv, f"📎 {comment}")
                await sender.send_file(conv, media_id, target.name, suffix)
                sent += 1
            except Exception as e:
                errors.append(f"{conv.address}: {e}")

        if sent == 0:
            detail = "; ".join(errors) if errors else "no conversations known"
            raise RuntimeError(f"file not sent: {detail}")
        result = f"sent '{target.name}' to {sent} conversation(s)"
        if errors:
            result += f" ({len(errors)} failed: {'; '.join(errors)})"
        return result

    return host


def _resolve_inside_root(root: Path, raw_path: str) -> Path:
    """把工具入参路径解析到共享根内，拒绝越界。"""
    root = root.resolve()
    candidate = Path(os.path.expanduser(raw_path))
    if not candidate.is_absolute():
        candidate = root / candidate
    resolved = candidate.resolve()
    if resolved != root and root not in resolved.parents:
        raise ValueError(f"path escapes shared root: {raw_path}")
    if not resolved.is_file():
        raise ValueError(f"not a file: {raw_path}")
    return resolved
