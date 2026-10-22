# wing/gateway/routes/settings.py — Setting API 端点

"""Setting API 的四个端点：``schema`` / ``get`` / ``status`` / ``set``。

RPC 风格（不是 RESTful）：目录、取值、状态、保存各自一个端点。几条写死的语义：

- **校验失败走 HTTP 200 + ``ok=false`` + problems**：
  请求本身合法，是用户填的内容不合法——这是业务结果不是协议错误。HTTP 错误码只留给
  协议级失败：409（指纹不匹配）/ 401·403（鉴权）/ 500（写盘失败）。
- **读端点不经过 ``server.runtime``**：setup mode 下 ``server.runtime`` 抛
  ``SetupModeError``，而 ``schema`` / ``get`` / ``status`` 必须仍可用（守门白名单）。
  它们由 ``config.document`` 的纯函数 + 投影组合而成；写路径（保存事务）仍住 runtime。
- ``setup_mode`` 是**真值**（取自 ``server.in_setup_mode``）：网关是否正以
  降级态运行——面板据此显示徽标；``valid`` 的口径因此是
  ``valid == (not setup_mode and 无 problem)``（降级态恒 false）。
  另两个字段写死：``version`` 复用 health 的版本解析（单一实现）；``config_path`` 恒为
  绝对路径（面板标题栏展示）。

Route handler 只做：读盘 → 调 runtime（仅 set）→ 构造响应。
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException, Request
from starlette.responses import JSONResponse

from wing.config import (
    ConfigProblem,
    build_catalog,
    build_groups,
    get_config_path,
)
from wing.config.document import (
    ConfigDocumentError,
    mask_secrets,
    problems_of,
    read_document,
    secret_states,
)
from wing.gateway.protocol import (
    ErrorResponse,
    SettingProblem,
    SettingsGetResponse,
    SettingsSchemaResponse,
    SettingsSetRequest,
    SettingsSetResponse,
    SettingsStatusResponse,
    error_response,
)
from wing.gateway.projection import (
    build_settings_apply_response,
    build_settings_problem,
    build_settings_schema,
    build_settings_secret_states,
)
from wing.gateway.routes._common import get_server
from wing.gateway.routes.health import _get_version
from wing.runtime import SettingsConflictError

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from wing.gateway.server import GatewayServer

router = APIRouter(tags=["settings"])


def _with_boot_problems(
    server: GatewayServer, problems: list[ConfigProblem]
) -> list[ConfigProblem]:
    """降级期把「启动为什么没起来」并进问题清单。

    ``read_document`` 的口径里有些形态（顶层非字符串键）是**未知键**——进 ``extra``
    原样保留、不算错误；但启动读取更严（``Config(**parsed)`` / 运行时装配），
    网关确实因为它没起来。不并进来的话用户会看到「invalid 但一条问题都没有」
    ——setup 首屏就是空清单。

    去重按 ``(path, kind)``（与面板的合并口径一致）：同一处问题磁盘视图能说得更细
    时以它为准（它描述的是**当前**文件），启动快照只补磁盘视图答不出来的那一类。
    """
    if not server.in_setup_mode:
        return problems
    merged = list(problems)
    seen = {(problem.path, problem.kind) for problem in merged}
    for problem in server.setup_problems:
        if (problem.path, problem.kind) in seen:
            continue
        merged.append(problem)
        seen.add((problem.path, problem.kind))
    return merged


def _projected_problems(
    server: GatewayServer, source: list[ConfigProblem]
) -> list[SettingProblem]:
    """问题 → wire 投影：并进降级期启动快照，再逐条转协议模型。

    两个读端点的四个分支（文档坏 / 文档好 × get / status）共用这一步，
    面板与预检因此不会对同一份文件给出不同结论。
    """
    return [build_settings_problem(p) for p in _with_boot_problems(server, source)]


@router.get(
    "/api/settings/schema",
    response_model=SettingsSchemaResponse,
    summary="获取设置目录（catalog 树）",
)
async def settings_schema() -> SettingsSchemaResponse:
    """设置目录：每个配置项的默认值 / 约束 / 枚举 / 生效域 / 密文标记 / 分组。

    纯静态（只依赖声明层），可长缓存；面板与 ``wing config`` 的全部素材都来自它。
    """
    return build_settings_schema(
        build_catalog(),
        version=_get_version(),
        config_path=str(get_config_path()),
        groups=build_groups(),
    )


@router.get(
    "/api/settings/get",
    response_model=SettingsGetResponse,
    summary="获取当前配置（稀疏文档 + 密文状态 + 问题）",
)
async def settings_get(request: Request) -> SettingsGetResponse:
    """现读磁盘 → 稀疏文档（密文叶子 = ``null``）+ 密文状态表 + 指纹 + 全部问题。

    真实密钥永不回显：掩码后的 ``values`` 只有本 **就是** ``null`` 的占位；
    末 4 位提示走 ``secrets`` 表。文件坏掉也照常应答（``values={}`` + 一条文档级问题），
    这是用户可修的状态，不是服务端故障。

    ``setup_mode`` 是**真值**：网关是否正以降级态运行——面板据此显示徽标。
    """
    server = get_server(request)
    setup_mode = server.in_setup_mode
    config_path = str(get_config_path())
    # 一次建树喂三处：读盘切分（read_document）、掩码 / 密文状态、问题清单。
    catalog = build_catalog()
    try:
        doc, fingerprint = read_document(catalog=catalog)
    except ConfigDocumentError as exc:
        return SettingsGetResponse(
            values={},
            secrets={},
            fingerprint=exc.fingerprint,
            problems=_projected_problems(server, [exc.as_problem()]),
            setup_mode=setup_mode,
            config_path=config_path,
        )

    return SettingsGetResponse(
        values=mask_secrets(doc, catalog),
        secrets=build_settings_secret_states(secret_states(doc, catalog)),
        fingerprint=fingerprint.value,
        problems=_projected_problems(server, problems_of(doc, catalog=catalog)),
        setup_mode=setup_mode,
        config_path=config_path,
    )


@router.get(
    "/api/settings/status",
    response_model=SettingsStatusResponse,
    summary="配置健康预检（最便宜的调用）",
)
async def settings_status(request: Request) -> SettingsStatusResponse:
    """``{valid, setup_mode, problems, fingerprint}``——TUI 启动路径上的预检。

    独立于 ``get`` 的理由：启动时不需要整个文档与密文状态表。

    ``valid`` 的口径（必须自洽）：``valid == (not setup_mode and 无 problem)``
    ——降级态**恒** ``false``（哪怕磁盘上的文件已经被外部改好：那一刻网关还没装配好，
    报 valid 会让预检把用户直接送进正常启动链，然后吃一串 503）。
    ``setup_mode`` 是**真值**。
    """
    server = get_server(request)
    setup_mode = server.in_setup_mode
    try:
        doc, fingerprint = read_document()
    except ConfigDocumentError as exc:
        return SettingsStatusResponse(
            valid=False,
            setup_mode=setup_mode,
            problems=_projected_problems(server, [exc.as_problem()]),
            fingerprint=exc.fingerprint,
        )

    problems = _projected_problems(server, problems_of(doc))
    return SettingsStatusResponse(
        valid=(not setup_mode) and not problems,
        setup_mode=setup_mode,
        problems=problems,
        fingerprint=fingerprint.value,
    )


@router.post(
    "/api/settings/set",
    response_model=SettingsSetResponse,
    responses={409: {"model": ErrorResponse, "description": "指纹不匹配（乐观并发）"}},
    summary="保存配置（校验 → 备份 → 原子写 → 热重载）",
)
async def settings_set(
    body: SettingsSetRequest, request: Request
) -> SettingsSetResponse | JSONResponse:
    """保存事务的入口——业务逻辑在 ``WingRuntime.apply_settings``（本 handler 只做转发）。

    - 校验失败 → **200 + ``ok=false`` + problems**（文件一个字节都没写）；
    - 指纹不匹配 → **409**（乐观并发；文件未被触碰）；
    - 写盘失败 → **500**（``OSError``：磁盘满 / 权限）。
    """
    server = get_server(request)
    try:
        result = await server.runtime.apply_settings(
            document=body.document, base=body.base
        )
    except SettingsConflictError as exc:
        return error_response(409, str(exc), error="conflict")
    except OSError as exc:
        raise HTTPException(
            status_code=500, detail=f"failed to write config.yaml: {exc}"
        )
    return build_settings_apply_response(result)
