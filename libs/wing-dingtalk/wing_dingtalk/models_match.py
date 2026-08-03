"""模型模糊匹配——`/model` 命令的核心逻辑（纯函数，可单测）。

规则：
- 裸名：跨 provider 全局匹配。精确命中优先；否则大小写不敏感子串匹配。
- `provider:model` 或 `provider/model`：首段若命中 provider 名（大小写
  不敏感），则限定在该 provider 内匹配——"聪明"的定向方式。
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class ModelRef:
    provider: str
    model: str

    def __str__(self) -> str:
        return f"{self.provider}:{self.model}"


@dataclass
class MatchResult:
    hit: ModelRef | None = None
    candidates: list[ModelRef] = field(default_factory=list)
    error: str | None = None

    @property
    def ok(self) -> bool:
        return self.hit is not None


def flatten(groups: list[dict]) -> list[ModelRef]:
    """/api/models 响应（providers 分组）→ 平铺 (provider, model) 列表。"""
    refs: list[ModelRef] = []
    for group in groups:
        provider = group.get("provider", "")
        for model in group.get("models", []):
            refs.append(ModelRef(provider=provider, model=model))
    return refs


def match_model(groups: list[dict], query: str) -> MatchResult:
    """模糊匹配模型。groups 为 /api/models 的 providers 列表。"""
    query = query.strip()
    if not query:
        return MatchResult(error="empty query")

    refs = flatten(groups)
    if not refs:
        return MatchResult(error="no models available from gateway")

    pool = refs
    # 定向 provider：首段是已知 provider 名时生效
    for sep in (":", "/"):
        if sep in query:
            head, _, rest = query.partition(sep)
            head, rest = head.strip(), rest.strip()
            provider = _find_provider(refs, head)
            if provider is not None and rest:
                pool = [r for r in refs if r.provider == provider]
                query = rest
            break

    # 1. 精确匹配（大小写不敏感）
    exact = [r for r in pool if r.model.lower() == query.lower()]
    if len(exact) == 1:
        return MatchResult(hit=exact[0])
    if len(exact) > 1:
        return MatchResult(candidates=exact)

    # 2. 子串匹配
    sub = [r for r in pool if query.lower() in r.model.lower()]
    if len(sub) == 1:
        return MatchResult(hit=sub[0])
    if len(sub) > 1:
        return MatchResult(candidates=sub)

    return MatchResult(error=f"no model matching '{query}'")


def _find_provider(refs: list[ModelRef], name: str) -> str | None:
    lowered = name.lower()
    for r in refs:
        if r.provider.lower() == lowered:
            return r.provider
    return None
