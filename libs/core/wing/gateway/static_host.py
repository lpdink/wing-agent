# wing/gateway/static_host.py — web 构建的静态托管策略（配置解析 / 保留路径 / 公开判定）

"""`gateway.static_dir` 的解析与「哪些路径永不 fallback / 哪些路径公开」的判定。

本模块只有策略与配置读取（一个 `is_dir()`，其余零 I/O）；响应构造在
`routes/static.py`，鉴权豁免在 `auth.py`——三处共用同一份保留路径清单，
避免"托管不 fallback 但鉴权放行"这类只有一边改了的漂移。
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

from wing.config import get_wing_home
from wing.gateway.file_policy import is_within

#: 永不 fallback、也永不吐静态文件的**精确**路径（框架路由与协议入口）。
RESERVED_EXACT: frozenset[str] = frozenset(
    {"/api", "/ws", "/docs", "/redoc", "/openapi.json"}
)

#: 同上，前缀形态（`/ws/` 也在内：`/ws` 下今天没有 HTTP 路由，但"保留"是子路径
#: 整体语义——否则 `/ws/anything` 会静默变成 SPA 路径并在 auth 开启时免鉴权）。
#: 新增非 `/api` 的 HTTP 端点时必须同步这里——否则该端点会被静态托管静默公开
#: （鉴权豁免只看这个清单）。由单测与文档双重钉住。
RESERVED_PREFIXES: tuple[str, ...] = ("/api/", "/docs/", "/redoc/", "/ws/")


def is_reserved_path(path: str) -> bool:
    """路径是否属于保留前缀（`/api`、`/docs`、`/redoc`、`/openapi.json`、`/ws`）。

    三条判定：精确匹配、前缀匹配、**保留精确路径的尾斜杠形态**。

    - 精确：`/api` 本身不能漏（否则 `GET /api` 会被 SPA fallback 吞成 200 的 HTML）；
    - 前缀：`/api/x` 与 `/apidocs` 必须区分（前缀串比较会误伤）；
    - 尾斜杠：`/openapi.json/` 属于"框架路径的另一种写法"，必须留给 Starlette 的
      `redirect_slashes` 分支回 307——catch-all 若认领它，重定向就变成 404
      （`/docs/`、`/api/health/` 由前缀规则天然覆盖，`/openapi.json/` 只能靠这条）。
    """
    if path in RESERVED_EXACT or path.startswith(RESERVED_PREFIXES):
        return True
    return path.endswith("/") and path.rstrip("/") in RESERVED_EXACT


def configured_static_dir(gateway_config: Any) -> str | None:
    """取 `static_dir` 的配置值（None / 非字符串 / 空白 = 未配置）。

    类型宽松是有意的：配置对象在测试里常是替身（MagicMock），这里必须把"没有这个
    配置"与"配置了个怪值"都收敛成"未配置"，而不是在请求路径上抛异常。
    """
    raw = getattr(gateway_config, "static_dir", None)
    if not isinstance(raw, str) or not raw.strip():
        return None
    return raw.strip()


def static_hosting_enabled(gateway_config: Any) -> bool:
    """是否配置了静态托管（**不**要求目录存在——目录缺失只是回 404）。"""
    return configured_static_dir(gateway_config) is not None


def resolve_static_root(gateway_config: Any) -> Path | None:
    """解析静态根目录（realpath）；未配置或目录不存在返回 None。

    - 绝对路径按原样；相对路径按配置所在目录（`$WING_HOME/core`）解析；
    - `~` 展开；
    - 目录不存在 = 未启用：不报错、不拖垮网关（web 还没构建时配置可以先行），
      且解析发生在每个请求上——产物事后出现即生效，无需重启。
    """
    raw = configured_static_dir(gateway_config)
    if raw is None:
        return None
    path = Path(raw).expanduser()
    if not path.is_absolute():
        path = get_wing_home() / path
    resolved = Path(os.path.realpath(path))
    return resolved if resolved.is_dir() else None


def is_public_static_path(gateway_config: Any, path: str) -> bool:
    """该路径是否对未鉴权客户端公开（web 壳资源）。

    条件：静态托管已配置 **且** 路径不在保留前缀内。壳本身不含数据；`/api/*`
    与框架路由（`/docs` 等）维持现状（auth 开启时仍要 key）。

    `path` 必须是**路由路径**（`starlette.routing.get_route_path`，已剥掉
    `root_path`）：反代挂子路径时用 `request.url.path` 会把 `/prefix/api/...`
    看成"非 `/api`"→ 放行，是实打实的 fail-open。调用点见 `auth.py`。
    """
    return static_hosting_enabled(gateway_config) and not is_reserved_path(path)


def sensitive_static_root_warning(gateway_config: Any) -> str | None:
    """静态根落在敏感目录（含 `$WING_HOME/core` 的目录树）时的启动警告；None = 无风险。

    静态托管开启后，根目录下的**所有**文件都会变成未鉴权可读（含 auth 开启时）。
    相对路径的锚点恰好是 `$WING_HOME/core`，因此最顺手的写法
    （`static_dir: "."`）会把 config.yaml / logs / sessions 一起端出去——
    `config.yaml` 里就躺着 provider api key。这里只警告，不改行为（运维可能是有意为之）。
    """
    root = resolve_static_root(gateway_config)
    if root is None:
        return None
    wing_home = get_wing_home().resolve()
    if not is_within(wing_home, root):
        return None
    return (
        f"gateway.static_dir resolves to {root}, which contains {wing_home} "
        "(config.yaml with provider keys / logs / sessions). Every file under "
        "the static root is publicly readable — even with gateway.auth enabled. "
        "Point static_dir at the web build directory instead."
    )
