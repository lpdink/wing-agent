"""Gateway 容器入口——从环境变量渲染 config.yaml 后启动 wing-gateway。

模板: /app/deploy/gateway/config.template.yaml（随镜像 COPY 到 /template.yaml）。
渲染结果只在变化时写盘（$WING_HOME/core/config.yaml）。
"""

from __future__ import annotations

import os
from pathlib import Path

TEMPLATE = Path("/template.yaml")


def main() -> None:
    home = Path(os.environ.get("WING_HOME", "/data/wing"))
    core = home / "core"
    core.mkdir(parents=True, exist_ok=True)
    (core / "commands").mkdir(exist_ok=True)

    rendered = os.path.expandvars(TEMPLATE.read_text(encoding="utf-8"))
    target = core / "config.yaml"
    if not target.exists() or target.read_text(encoding="utf-8") != rendered:
        target.write_text(rendered, encoding="utf-8")

    os.execvp("wing-gateway", ["wing-gateway"])


if __name__ == "__main__":
    main()
