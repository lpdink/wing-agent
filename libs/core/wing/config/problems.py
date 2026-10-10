# wing/config/problems.py
"""跨字段检查的抽取：一条检查一处实现，喂两个消费方（加载期 / 设置面板）。

为什么单独成模块：

- 加载期只需要**第一个** problem（pydantic raise 契约，文案是逐字红线）；
  设置面板 / ``wing config doctor`` 需要**全部** problem，且要定位到精确路径
  （``providers[0].models[1].name``）。抽取成纯函数后，同一批检查喂两个消费方，
  不允许第二份实现（第二份 = 新的 SYNC 纪律）。
- 纯逻辑：不读文件、不 import gateway / runtime、不抛异常（输入再脏也只少报不漏报）。

两种输入形态（同一批检查都要能用）：

1. **已构造的 ``Config``**（加载期路径）：嵌套模型均为实例，字段级校验已经通过；
2. **宽容视图 ``Config.model_construct(**raw)``**（设置面板路径）：跨字段有问题时
   ``Config(**raw)`` 根本构造不出来，面板只能拿不校验的视图。视图里嵌套值可能是原始
   ``dict`` / ``str`` / 任意脏值——凡是**无法判定**的形态一律跳过：字段级校验
   （03 的 ``document.locate_problems``）负责报它们，这里不重复也不猜。

顺序即「加载期第一个 problem」的优先级，见 :func:`cross_field_problems`。
"""

from __future__ import annotations

from collections.abc import Iterator, Mapping, Sequence
from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING, Any, NamedTuple

if TYPE_CHECKING:
    from .models import Config

UNKNOWN_ID_LIST_LIMIT = 10
"""错误文案里 available ids 的展示上限（超出截断为前 N 个 + ``…``）。"""


class ProblemKind(str, Enum):
    """问题的机器可读分类（面板图标 / 严重度排序 / CLI 过滤）。"""

    MISSING_REQUIRED = "missing_required"
    INVALID_VALUE = "invalid_value"
    EMPTY_LIST = "empty_list"
    DUPLICATE = "duplicate"
    UNKNOWN_REFERENCE = "unknown_reference"
    CONFLICT = "conflict"
    UNKNOWN_KEY = "unknown_key"


@dataclass(frozen=True)
class ConfigProblem:
    """一条配置问题。

    ``message`` 是**加载期 raise 的逐字文案**（见 :meth:`render`）；``path`` / ``hint``
    只服务面板与 CLI，绝不进入加载期文案——那是 ``test_config_problems.py`` 钉住的红线。
    """

    path: str | None
    """规范路径（``providers[0].models[1].name``）；``None`` = 文档级（无法定位到单个字段）。"""
    kind: ProblemKind
    """问题分类。"""
    message: str
    """人类可读文案（与加载期 raise 的文本逐字一致）。"""
    hint: str | None = None
    """可操作建议（面板 footer / CLI 提示）；不参与 :meth:`render`。"""

    def render(self) -> str:
        """加载期文案（= ``message``）。

        ``Config._validate_config`` 用它喂 ``raise ValueError(...)``：输出必须与抽取前
        逐字一致，所以 ``path`` / ``hint`` 一律不参与渲染。
        """
        return self.message


class ModelRefView(NamedTuple):
    """模型引用词视图（id / 调用名 / provider 名）。

    跨字段检查与 C7 错误文案只需要这三个字段；它让同一批检查既能吃 ``ModelRef``
    （已构造的 ``Config``），也能吃原始 ``dict``（宽容视图）。
    """

    id: str
    name: str
    provider_name: str


def model_name_message(name: Any) -> str | None:
    """调用名守门（非空 / 无首尾空白）的文案；合法或无法判定时返回 ``None``。

    这是该规则的**唯一实现**：``ModelSpec._name_valid``（字段校验器，加载期由
    ``model_names()`` 对裸字符串形态触发）与跨字段检查共用——抄两份就是第二份
    真相。合法性的语义：``name.strip()`` 非空，且 ``name == name.strip()``。
    """
    if not isinstance(name, str):
        return None
    if not name.strip():
        return "model name must be non-empty"
    if name != name.strip():
        return f"model name must not have leading/trailing whitespace, got: '{name}'"
    return None


def unknown_model_message(refs: Sequence[ModelRefView], model_id: str) -> str:
    """未命中 id 的自解释文案（C7）——纯函数，不含副作用。

    形如::

        unknown model id 'sonnet'; available ids: ds-flash, ds-pro, gpt-4o, …;
        note: 'sonnet' is the call name of model id 'ds-flash' (provider 'local') —
        declare an explicit id or send 'ds-flash'

    available ids 按声明序、超过 ``UNKNOWN_ID_LIST_LIMIT`` 截断；请求值命中某声明的
    **调用名**时给出「它就是哪个 id 的调用名」提示。这只是错误路径的提示，
    **绝不自动生效**（不是 resolve——那正是模型 id 任务要消灭的东西）。

    这是该文案的唯一实现：``Config.describe_unknown_model`` 委托到这里。
    """
    ids = [ref.id for ref in refs]
    shown = ", ".join(ids[:UNKNOWN_ID_LIST_LIMIT])
    if len(ids) > UNKNOWN_ID_LIST_LIMIT:
        shown = f"{shown}, …"
    parts = [f"unknown model id '{model_id}'; available ids: {shown}"]

    wanted = model_id.strip()
    hints = [ref for ref in refs if ref.name == wanted]
    if hints:
        noun = "model id" if len(hints) == 1 else "model ids"
        described = " and ".join(
            f"'{ref.id}' (provider '{ref.provider_name}')" for ref in hints
        )
        target = f"send '{hints[0].id}'" if len(hints) == 1 else "send one of those ids"
        parts.append(
            f"note: '{model_id}' is the call name of {noun} {described} — "
            f"declare an explicit id or {target}"
        )
    return "; ".join(parts)


# ─────────────────────────────────────────────────────────────────────────────
# 宽容读取（已构造的模型 / model_construct 出来的原始视图，两种都读）
# ─────────────────────────────────────────────────────────────────────────────


def _field(target: Any, name: str) -> Any:
    """宽容读字段：模型走属性，原始 ``dict`` 走键；都不是则 ``None``。"""
    if isinstance(target, Mapping):
        return target.get(name)
    return getattr(target, name, None)


def _items(value: Any) -> list[Any]:
    """宽容取序列：非 ``list`` / ``tuple``（含 ``None`` / 标量）一律视为「无法判定」→ 空。"""
    if isinstance(value, (list, tuple)):
        return list(value)
    return []


def _spec_view(spec: Any, provider_name: str) -> ModelRefView | None:
    """一条模型声明（裸字符串 / dict / ``ModelSpec``）→ 引用词视图；无法判定时 ``None``。

    调用名非法的声明（空 / 首尾空白）**不算目录的一部分**：非法形态由 ``ModelSpec``
    的字段校验器负责报（加载期经 ``model_names()`` 触发），这里跳过它，免得
    「未命中」文案里出现一个读不通的 available ids 列表。
    """
    if isinstance(spec, str):
        if model_name_message(spec) is not None:
            return None
        return ModelRefView(id=spec, name=spec, provider_name=provider_name)
    name = _field(spec, "name")
    if not isinstance(name, str) or model_name_message(name) is not None:
        return None
    spec_id = _field(spec, "id")
    if spec_id is not None and not isinstance(spec_id, str):
        return None
    return ModelRefView(id=spec_id or name, name=name, provider_name=provider_name)


def _provider_name(provider: Any) -> str | None:
    """provider 的 name（非字符串 = 无法判定，依赖它的检查一律跳过）。"""
    name = _field(provider, "name")
    return name if isinstance(name, str) else None


def _specs(provider: Any) -> list[tuple[Any, ModelRefView | None]]:
    """``(原始声明项, 视图|None)`` 列表，按声明序。"""
    provider_name = _provider_name(provider) or ""
    return [
        (spec, _spec_view(spec, provider_name))
        for spec in _items(_field(provider, "models"))
    ]


def iter_model_refs(providers: Any) -> Iterator[ModelRefView]:
    """按声明序产出全部引用词视图（provider 声明序 × 模型声明序）。"""
    for provider in _items(providers):
        for _, view in _specs(provider):
            if view is not None:
                yield view


def _model_path(prefix: str, index: int, spec: Any) -> str:
    """模型声明项的规范路径：对象形态指到 ``.name``，裸字符串形态就是条目本身。"""
    return f"{prefix}models[{index}]" + ("" if isinstance(spec, str) else ".name")


def _duplicate_positions(values: Sequence[str | None]) -> list[int]:
    """重复值**第二次出现**（及以后，每个名字只报一次）的下标，按出现顺序。

    与逐名扫描的 raise 语义同序：``['a','b','a','b']`` → ``[2, 3]``（``[0] = 2`` 即
    今天 raise 的那一处）。
    """
    seen: set[str] = set()
    reported: set[str] = set()
    out: list[int] = []
    for index, value in enumerate(values):
        if value is None:
            continue
        if value in seen and value not in reported:
            out.append(index)
            reported.add(value)
        seen.add(value)
    return out


# ─────────────────────────────────────────────────────────────────────────────
# 检查
# ─────────────────────────────────────────────────────────────────────────────


def provider_model_problems(
    provider: Any, *, path_prefix: str = ""
) -> list[ConfigProblem]:
    """单个 provider 的**内部**跨字段检查：同一 provider 内实际调用名不得重复。

    重复声明在运行期表现为「同一模型两套能力 / 展示」，属配置错误，必须在解析期报出。
    ``path_prefix`` 由调用方给（``cross_field_problems`` 传 ``"providers[0]."``）；
    独立构造的 ``ProviderConfig`` 没有文档路径，传空串（路径相对该 provider）。
    """
    problems: list[ConfigProblem] = []
    specs = _specs(provider)
    names = [view.name if view is not None else None for _, view in specs]
    for index in _duplicate_positions(names):
        problems.append(
            ConfigProblem(
                path=_model_path(path_prefix, index, specs[index][0]),
                kind=ProblemKind.DUPLICATE,
                message=f"duplicate model name: '{names[index]}'",
                hint="同一个 provider 内两条声明的调用名不能重复",
            )
        )
    return problems


def cross_field_problems(config: Config) -> list[ConfigProblem]:
    """``Config`` 的全部跨字段问题（不抛异常）。

    返回顺序 = 加载期「第一个 problem」的优先级（``Config._validate_config`` 只 raise
    ``problems[0].render()``）：

    1. 逐 provider：同一 provider 内调用名重复——它是**嵌套模型**校验，pydantic 路径里
       先于 Config 级检查执行，所以排在最前；
    2. ``agents`` 为空（今天的检查顺序：两个都空时先报 agents）；
    3. ``providers`` 为空；
    4. provider name 重复；
    5. agent name 重复；
    6. 逐 provider：``models`` 为空 → 随后逐声明：model id 全局重复（同一 provider 内撞 id
       的文案说一遍 provider 名，跨 provider 说两遍）；
    7. 逐 agent：``agents[].model`` 未命中 id 空间（文案 = ``unknown_model_message``）。
    """
    problems: list[ConfigProblem] = []
    providers = _items(_field(config, "providers"))
    agents = _items(_field(config, "agents"))

    # ① provider 内部：调用名重复（嵌套校验，故先于 Config 级检查）。
    for index, provider in enumerate(providers):
        problems.extend(
            provider_model_problems(provider, path_prefix=f"providers[{index}].")
        )

    # ② / ③ 显式「不得为空」：声明层的 min_items=1 是同一事实的机器可读镜像
    # （两者的一致性由 test_config_problems.py 钉住）。
    if not agents:
        problems.append(
            ConfigProblem(
                path="agents",
                kind=ProblemKind.EMPTY_LIST,
                message="agents list cannot be empty",
                hint="至少声明一个 agent，并让它引用某个 model id",
            )
        )
    if not providers:
        problems.append(
            ConfigProblem(
                path="providers",
                kind=ProblemKind.EMPTY_LIST,
                message="providers list cannot be empty",
                hint="至少声明一个 provider（模型目录的唯一来源）",
            )
        )

    # ④ / ⑤ 名字唯一性（每个重复名字报第二次出现处）。
    for index in _duplicate_positions([_provider_name(p) for p in providers]):
        problems.append(
            ConfigProblem(
                path=f"providers[{index}].name",
                kind=ProblemKind.DUPLICATE,
                message=f"duplicate provider name: '{_provider_name(providers[index])}'",
                hint="重命名其中一个 provider",
            )
        )
    agent_names: list[str | None] = []
    for agent in agents:
        name = _field(agent, "name")
        agent_names.append(name if isinstance(name, str) else None)
    for index in _duplicate_positions(agent_names):
        problems.append(
            ConfigProblem(
                path=f"agents[{index}].name",
                kind=ProblemKind.DUPLICATE,
                message=f"duplicate agent name: '{agent_names[index]}'",
                hint="重命名其中一个 agent",
            )
        )

    # ⑥ 模型目录 = 配置声明的静态投影：每个 provider 至少声明一个模型，且
    # effective id 跨 provider 全局唯一（「全局唯一」是「解析 = 单键查表」的前提：
    # 没有候选集合、没有优先级、没有回落）。
    declared_by: dict[str, str | None] = {}
    for index, provider in enumerate(providers):
        provider_name = _provider_name(provider)
        specs = _specs(provider)
        if not specs:
            if provider_name is not None:
                problems.append(
                    ConfigProblem(
                        path=f"providers[{index}].models",
                        kind=ProblemKind.MISSING_REQUIRED,
                        message=(
                            f"provider '{provider_name}' declares no models: "
                            "providers[].models must declare at least one model "
                            "(the model catalog comes from configuration only)"
                        ),
                        hint="给该 provider 至少声明一个模型（裸字符串或对象形态）",
                    )
                )
            continue
        for spec_index, (spec, view) in enumerate(specs):
            if view is None:
                continue  # 声明形态无法判定：字段级校验的地盘
            if view.id not in declared_by:
                declared_by[view.id] = provider_name
                continue
            owner_name = declared_by[view.id]
            if provider_name is None or owner_name is None:
                continue  # 名字不是字符串：组织不出冲突文案，跳过
            if owner_name == provider_name:
                # 同一 provider 内两条声明撞 id（名字不同、id 显式撞车）：
                # 说两遍 provider 名会读成 bug。
                conflict = (
                    f"duplicate model id '{view.id}' declared twice "
                    f"by provider '{provider_name}'."
                )
            else:
                conflict = (
                    f"duplicate model id '{view.id}': declared by provider "
                    f"'{owner_name}' and provider '{provider_name}'."
                )
            explicit_id = _field(spec, "id")
            suffix = (
                "" if isinstance(spec, str) else (".id" if explicit_id else ".name")
            )
            problems.append(
                ConfigProblem(
                    path=f"providers[{index}].models[{spec_index}]{suffix}",
                    kind=ProblemKind.DUPLICATE,
                    message=(
                        f"{conflict}\n"
                        "Give one an explicit id, e.g.:\n"
                        f"  - id: {provider_name}-{view.name}\n"
                        f"    name: {view.name}"
                    ),
                    hint="给其中一个声明显式 id（id 是全局唯一引用词）",
                )
            )

    # ⑦ agents[].model 必须落在 id 空间内（未命中即配置错误——文案即 C7：available ids
    # + 调用名提示，外部编排方据此一次改对）。校验与解析共用同一 trim 语义：
    # find_model() 能命中的值，加载期就放行——「加载说合法 ⇔ 解析能命中」由构造保证。
    #
    # 目录为空（一条合法声明都没有）时不报「未命中」：目录本身已经被报成问题
    # （无模型 / 声明非法），此时 available ids 是空列表，报未命中没有信息量。
    # 加载期不会走到这里——那条路径在 ⑥ 就 raise 了。
    refs = list(iter_model_refs(providers))
    if refs:
        for index, agent in enumerate(agents):
            model_id = _field(agent, "model")
            if not isinstance(model_id, str):
                continue
            wanted = model_id.strip()
            if any(ref.id == wanted for ref in refs):
                continue
            problems.append(
                ConfigProblem(
                    path=f"agents[{index}].model",
                    kind=ProblemKind.UNKNOWN_REFERENCE,
                    message=unknown_model_message(refs, model_id),
                    hint="把 agents[].model 改成上面列出的某个 id",
                )
            )

    return problems
