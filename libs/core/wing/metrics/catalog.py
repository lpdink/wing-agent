# wing/metrics/catalog.py — 会话目录（``session_catalog``）的写入与回填
"""``session_catalog`` 是磁盘会话元数据的**派生投影**：磁盘是事实源，本表可随时重建。

写法只有两种，且都是 ``INSERT OR IGNORE``：

- :func:`record_session_created`——新建会话时写一次（之后永不更新：workspace / 模板 /
  父都不变，``model_id`` 是**创建时刻**的绑定而不是当前模型，标题由前端 join）；
- :func:`backfill_catalog`——网关启动后的后台补齐，只处理「磁盘上有、目录里没有」的
  会话（存量会话 + 运行期被拷进来的会话目录）。

创建时刻没有权威来源（系统里从来没有``created_at``），因此按可靠性三档如实标注
``created_source``：``id``（id 前缀 ``YYYYMMDD-HHMMSS``，秒级）→ ``log``
（``history.jsonl`` 首条记录的时刻，毫秒）→ ``meta``（``last_interaction``，兜底）。
三档都拿不到就**跳过**——不发明时间，这一行留给下一次回填。

写失败绝不影响 agent：``wing.db`` 降级时 ``execute`` 返回 False，本层再兜一层
``try/except``（与 ``audit/recorder.py`` 同一条纪律）。
"""

from __future__ import annotations

from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from datetime import datetime

from wing.common.logger import log
from wing.common.utils import parse_session_id_time
from wing.db import Database, get_database
from wing.store import SessionStore

_INSERT = """
INSERT OR IGNORE INTO session_catalog (
    session_id, created_at_ms, created_ts, created_source,
    parent_session_id, forked_from, workspace, template_name, model_id
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
"""

_KNOWN_IDS = "SELECT session_id FROM session_catalog"


def _stamp(moment: datetime) -> tuple[int, str]:
    """与 ``audit/recorder.py`` 同一口径：``(epoch 毫秒, 本地 naive ISO)``。"""
    return int(moment.timestamp() * 1000), moment.isoformat()


def record_session_created(
    session_id: str,
    *,
    parent_session_id: str | None = None,
    forked_from: str | None = None,
    workspace: str | None = None,
    template_name: str | None = None,
    model_id: str | None = None,
    database: Database | None = None,
) -> None:
    """登记一个**新建**会话（``INSERT OR IGNORE``：同 id 的既有行不被覆盖）。

    调用点必须是「创建新会话」的那几条分支（新建 / fork）。create-or-adopt 命中既有
    会话、以及 resume，都**不写**——目录行是创建动作的一次性事实（与「血缘只写一次」
    同口径），存量会话由 :func:`backfill_catalog` 补齐。
    """
    ts_ms, ts = _stamp(datetime.now())
    db = database if database is not None else get_database()
    try:
        db.execute(
            _INSERT,
            (
                session_id,
                ts_ms,
                ts,
                "create",
                parent_session_id,
                forked_from,
                workspace,
                template_name,
                model_id,
            ),
        )
    except Exception as exc:  # 兜底：统计失败不影响 agent
        log.error(f"session_catalog: failed to record {session_id!r}: {exc!r}")


@dataclass(frozen=True, slots=True)
class _Candidate:
    """磁盘上的一个候选会话（目录行的素材 + 它是不是 live 区里的）。"""

    session_id: str
    parent_session_id: str | None = None
    forked_from: str | None = None
    workspace: str | None = None
    template_name: str | None = None
    model_id: str | None = None
    last_interaction: str | None = None
    live: bool = True


def backfill_catalog(
    stores: Iterable[SessionStore], *, database: Database | None = None
) -> int:
    """把「磁盘上有、目录里没有」的会话补进 ``session_catalog``。

    单个会话失败不影响其它（逐条兜住 + 一行 WARNING）；整体跑完留一条 INFO。

    Returns:
        本次补齐的行数（差集语义：重跑返回 0）。
    """
    db = database if database is not None else get_database()
    known = _known_ids(db)
    added = 0
    for store in stores:
        for candidate in _candidates(store):
            if candidate.session_id in known:
                continue
            try:
                if _backfill_one(db, store, candidate):
                    known.add(candidate.session_id)
                    added += 1
            except Exception as exc:  # 一个坏会话不该中断整轮回填
                log.warning(
                    f"session_catalog: cannot backfill {candidate.session_id!r}: "
                    f"{exc!r}"
                )
    if added:
        log.info(f"session_catalog: backfilled {added} session row(s)")
    return added


# ── 内部 ─────────────────────────────────────────


def _known_ids(db: Database) -> set[str]:
    return {row[0] for row in db.query(_KNOWN_IDS) if isinstance(row[0], str)}


def _backfill_one(db: Database, store: SessionStore, candidate: _Candidate) -> bool:
    """写一行（创建时刻解析不出来就跳过——不发明时间）。"""
    created = _created_at(store, candidate)
    if created is None:
        return False
    (ts_ms, ts), source = created
    return db.execute(
        _INSERT,
        (
            candidate.session_id,
            ts_ms,
            ts,
            source,
            candidate.parent_session_id,
            candidate.forked_from,
            candidate.workspace,
            candidate.template_name,
            candidate.model_id,
        ),
    )


def _created_at(
    store: SessionStore, candidate: _Candidate
) -> tuple[tuple[int, str], str] | None:
    """创建时刻 + 它的精度来源（``id`` / ``log`` / ``meta``）；都没有 → ``None``。"""
    from_id = parse_session_id_time(candidate.session_id)
    if from_id is not None:
        return _stamp(from_id), "id"
    if candidate.live:
        from_log = _log_start(store, candidate.session_id)
        if from_log is not None:
            return _stamp(from_log), "log"
    if candidate.last_interaction is not None:
        try:
            from_meta = datetime.fromisoformat(candidate.last_interaction)
        except ValueError:
            from_meta = None
        if from_meta is not None:
            return _stamp(from_meta), "meta"
    return None


def _log_start(store: SessionStore, session_id: str) -> datetime | None:
    """``history.jsonl`` 首条记录的时刻（只读这一条：流的第一个元素就够）。"""
    try:
        first = next(iter(store.open_log(session_id).iter_all()), None)
    except OSError:
        return None
    if not isinstance(first, dict):
        return None
    for key in ("ts", "created_at"):
        value = first.get(key)
        if isinstance(value, str):
            try:
                return datetime.fromisoformat(value)
            except ValueError:
                continue
    return None


def _candidates(store: SessionStore) -> Iterator[_Candidate]:
    """磁盘上的候选会话：live 区（可读 ``metadata`` / 日志）+ 归档区（只读 metadata）。"""
    try:
        for summary in store.list_summaries():
            metadata = summary.metadata
            yield _Candidate(
                session_id=summary.id,
                parent_session_id=metadata.parent_session_id,
                forked_from=metadata.forked_from,
                workspace=metadata.workspace,
                template_name=metadata.template_name,
                model_id=metadata.model_id,
                last_interaction=metadata.last_interaction,
                live=True,
            )
    except (OSError, ValueError) as exc:  # 一个后端的目录读不动不该中断别的后端
        log.warning(f"session_catalog: cannot list sessions: {exc!r}")
    for archived in _archived(store):
        yield archived


def _archived(store: SessionStore) -> Iterator[_Candidate]:
    """归档区条目（不具备归档能力的后端 → 空）。

    归档会话的父 / fork 源只存在于它自己的 ``metadata.json`` 里（归档索引不存这两列），
    因此逐个读一次 metadata——归档列表本身是低频路径。
    """
    try:
        entries = store.list_archived()
    except NotImplementedError:
        return
    except (OSError, ValueError) as exc:
        log.warning(f"session_catalog: cannot list archived sessions: {exc!r}")
        return
    for entry in entries:
        metadata = store.load_archived_metadata(entry.id)
        if metadata is None:
            yield _Candidate(
                session_id=entry.id,
                workspace=entry.workspace,
                template_name=entry.template_name,
                model_id=entry.model_id,
                last_interaction=entry.last_interaction,
                live=False,
            )
            continue
        yield _Candidate(
            session_id=entry.id,
            parent_session_id=metadata.parent_session_id,
            forked_from=metadata.forked_from,
            workspace=metadata.workspace,
            template_name=metadata.template_name,
            model_id=metadata.model_id,
            last_interaction=metadata.last_interaction,
            live=False,
        )
