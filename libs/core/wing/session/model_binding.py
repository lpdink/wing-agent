# wing/session/model_binding.py
"""会话记录的**生效模型绑定**——冷列表与 resume 共用纯判定。

`Session._restore_persisted_model` 是「记录 → agent」的唯一**写**路径（resume /
逐出后水合时把模型三元组装回 agent）。只读面（`SessionManager.list_sessions`
的模型字段）走不了那条路——它不该创建 / 改动会话——但必须给出同一个答案：
「这个会话跑在哪个模型上」对用户是同一件事。本模块把那条优先级链做成纯函数：

1. ``model_id`` 命中 id 空间 → **当前映射**（``name`` / ``provider`` 跟随配置
   演化，id 不变而调用名改了是配置作者的正常操作）；
2. 未命中（id 被删 / 旧记录无 id）→ 记录里的快照 ``(provider_name, model_name)``
   （provider 仍可解析时；id 经 ``Config.identify`` 反查补全，可能仍为 None）；
3. 其余（无记录 / 记录不完整 / 快照 provider 不可用）→ **模板默认**。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

from wing.config import Config, ProviderConfig, resolve_model_display_name
from wing.store import SessionMetadata

from .template import AgentTemplate


@dataclass(frozen=True)
class ModelBinding:
    """会话的生效模型：``model_id`` 是身份，``model_name`` / ``provider_name`` 是
    运行期事实，``model_display_name`` 是展示素材（未声明 = None）。

    与 ``/api/session/info`` 的模型四件套同源同刻：``model_id`` 是引用词（∈ 配置
    声明的 id 空间；陈旧数据下可为 None），``model_name`` 是发给上游的调用名，
    ``provider_name`` 是承载它的 provider。展示名不是身份——任何匹配仍以 id 为准。
    """

    model_id: str | None
    model_name: str
    provider_name: str
    model_display_name: str | None = None


@dataclass(frozen=True)
class RecordedModel:
    """可恢复的模型及其来源。已解析的 provider 可供展示名投影复用。"""

    source: Literal["id", "snapshot"]
    model_id: str | None
    model_name: str
    provider_name: str
    provider_config: ProviderConfig | None = None


@dataclass(frozen=True)
class UnavailableSnapshotProvider:
    detail: str


def resolve_recorded_model(
    metadata: SessionMetadata, config: Config
) -> RecordedModel | UnavailableSnapshotProvider | None:
    """解析记录绑定，返回 provider 诊断或以 None 表示缺少完整记录。"""
    if metadata.model_id is not None:
        ref = config.find_model(metadata.model_id)
        if ref is not None:
            return RecordedModel("id", ref.id, ref.name, ref.provider_name)

    model, provider = metadata.model_name, metadata.provider_name
    if model is None or provider is None:
        return None
    try:
        provider_cfg = config.get_provider(provider)
    except ValueError as exc:
        return UnavailableSnapshotProvider(str(exc))
    return RecordedModel(
        "snapshot", config.identify(provider, model), model, provider, provider_cfg
    )


def resolve_model_binding(
    metadata: SessionMetadata,
    config: Config,
    template: AgentTemplate,
) -> ModelBinding:
    """为冷列表投影生效绑定，记录不可恢复时使用会话模板。"""
    recorded = resolve_recorded_model(metadata, config)
    if isinstance(recorded, RecordedModel):
        model_id, model, provider_name = (
            recorded.model_id,
            recorded.model_name,
            recorded.provider_name,
        )
        provider = recorded.provider_config
    else:
        model_id, model, provider_name = (
            template.model_id,
            template.model,
            template.provider_name,
        )
        provider = None
    if provider is None:
        try:
            provider = config.get_provider(provider_name)
        except ValueError:
            pass
    return ModelBinding(
        model_id=model_id,
        model_name=model,
        provider_name=provider_name,
        model_display_name=resolve_model_display_name(provider, model),
    )
