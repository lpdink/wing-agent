#!/usr/bin/env python3
"""速度演示用的长语料：一份（虚构的）任务队列设计文档，确定性生成。

为什么自己生成而不是拿真实 session 的文本：这些字会出现在 README 的 GIF 里，
必须是**可公开**的内容，而且四种速率要灌同一份语料（除了速度，画面上没有别的变量）。

它同时是渲染的压力素材：标题 / 段落 / 中英混排 / 行内代码 / 多语言代码块 /
表格 / 引用 / 公式，把 TUI 的 markdown 与高亮路径都喂到。

    uv run python scripts/demo/corpus.py --bytes 260000 > /tmp/corpus.md
"""

from __future__ import annotations

import argparse
import random

TITLE = """# Designing a reliable job queue

A job queue looks like a solved problem until the first worker dies mid-task, the
broker loses a partition, or a retry storm takes the database down. This is the
design we settled on after three rewrites — a lease-based dispatcher on top of
Postgres, with `idempotency keys` at the edge and a bounded retry budget.

The model is deliberately small:

- a **job** is a row with a state machine and a deadline;
- a **lease** is a time-bounded claim on a job, renewed by the worker;
- a **dispatcher** decides what to run next, and never blocks on a worker;
- a **sweeper** reclaims leases whose deadline has passed.

Everything else — priority, fairness, backpressure — is a policy on top of those
four pieces."""

PARAGRAPHS = [
    """The dispatcher is a single SQL statement plus a fairness queue. It scans a
partial index ordered by `run_at`, skips jobs whose `lease_until` is in the
future, and claims a batch with `FOR UPDATE SKIP LOCKED`. Skipping locked rows is
what makes N dispatchers safe to run at once: contention turns into parallelism
instead of a queue of blocked readers.""",
    """Leases carry a deadline rather than a heartbeat flag. A worker that stops
renewing does not need to be declared dead by anyone: the deadline simply passes,
and the sweeper puts the job back. This removes the classic failure mode where
the detector and the worker disagree about liveness while the job runs twice.""",
    """Retries are bounded by *attempts*, not by wall-clock time, and the delay is
exponential with full jitter. Full jitter matters more than the base: without it,
a thousand jobs that failed together come back together, and the retry wave is
indistinguishable from an outage.""",
    """Ordering is per-key, not global. Jobs that share a `key` (an account, a
document, a device) run one at a time in submission order; everything else runs
in parallel. Global ordering buys nothing a queue user actually wants and costs
you the ability to scale out.""",
    """We keep the state machine in the row, not in a side table. `state`,
`attempts`, `lease_until`, `run_at`, `last_error` — five columns that answer
"what is happening to this job" without a join. The history lives in an append-only
`job_events` table that nothing on the hot path reads.""",
    """Backpressure is a first-class signal. When the queue depth for a key crosses a
threshold, the *enqueue* path starts returning `429` with a `Retry-After` header.
Shedding at the edge is the only shed that is cheap; once a job is accepted you
owe the caller a result.""",
    """The sweeper runs on a jittered schedule (every 5s ± 1s), takes `LIMIT 500`, and
does one thing: `UPDATE jobs SET state = 'ready' WHERE state = 'leased' AND
lease_until < now()`. It is idempotent, it is safe to run on every replica, and
it never inspects the payload.""",
    """Deadlines are absolute timestamps on the row, not durations computed at read
time. Absolute timestamps survive process restarts, clock adjustments inside the
window, and the temptation to "just add five minutes" during an incident.""",
]

BULLETS = [
    """Three rules keep the scheduler honest:

1. **Claim, then work.** A job is never executed without a lease row.
2. **Renew or die.** A lease has a deadline; the worker renews at half the TTL.
3. **One writer per key.** Parallelism comes from keys, not from hope.""",
    """Failure modes we explicitly designed for:

- worker crashes between claim and execution (lease expires, job retried);
- worker crashes *after* the side effect but before the ack (idempotency key at
  the edge makes the retry a no-op);
- broker partition split (the lease table is the only authority);
- clock skew between replicas (deadlines use `now()` from the database);
- retry storms (full jitter, plus a per-key concurrency cap).""",
    """Operational notes:

- `pg_stat_activity` tells you more than any dashboard we built;
- the partial index is `WHERE state IN ('ready','leased')` — keep it small;
- vacuum the `job_events` table from a replica, never from the writer;
- alert on *lease expiry rate*, not on queue depth: depth is a demand signal,
  expiry rate is a bug signal.""",
]

PYTHON = '''```python
async def claim(conn, worker: str, limit: int = 16) -> list[Job]:
    """Claim up to *limit* jobs for *worker*; returns [] when the queue is quiet."""
    rows = await conn.fetch(
        """
        WITH due AS (
            SELECT id FROM jobs
             WHERE state = 'ready' AND run_at <= now()
             ORDER BY priority DESC, run_at
             LIMIT $2
             FOR UPDATE SKIP LOCKED
        )
        UPDATE jobs SET state = 'leased', lease_until = now() + $3::interval,
                        attempts = attempts + 1, worker = $1
          FROM due WHERE jobs.id = due.id
        RETURNING jobs.id, jobs.key, jobs.payload, jobs.attempts
        """,
        worker, limit, lease_ttl,
    )
    return [Job(**dict(r)) for r in rows]
```'''

RUST = """```rust
/// Renewal: the database clock is the authority, not the worker's wall clock.
pub async fn renew(&self, job_id: JobId) -> Result<Deadline, RenewError> {
    let row = sqlx::query_as::<_, LeaseRow>(
        r#"
        UPDATE jobs
           SET lease_until = now() + $2::interval
         WHERE id = $1 AND state = 'leased' AND worker = $3
        RETURNING lease_until
        "#,
    )
    .bind(job_id)
    .bind(self.ttl)
    .bind(&self.worker)
    .fetch_optional(&self.pool)
    .await
    .map_err(RenewError::Backend)?;

    match row {
        // Someone else claimed the job before we renewed: stop before the side effect.
        None => Err(RenewError::LeaseLost(job_id)),
        Some(row) => Ok(Deadline::from(row.lease_until)),
    }
}
```"""

TYPESCRIPT = """```ts
export function backoff(attempts: number, base = 250, cap = 30_000): number {
  const window = Math.min(cap, base * 2 ** Math.max(0, attempts - 1));
  return Math.random() * window; // full jitter — without it retries arrive in waves
}

export async function withRetry<T>(fn: () => Promise<T>, attempts = 5): Promise<T> {
  let last: unknown;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    try {
      return await fn();
    } catch (error) {
      last = error;
      if (!isTransient(error)) throw error;
      await sleep(backoff(attempt));
    }
  }
  throw new RetryExhausted(attempts, { cause: last });
}
```"""

SQL = """```sql
-- The hot index: only rows that still have work to do — keep it small and warm.
CREATE INDEX CONCURRENTLY jobs_due_idx
    ON jobs (priority DESC, run_at)
    WHERE state IN ('ready', 'leased');

-- The renew/reclaim race is settled by this WHERE clause:
-- only the current lease holder renews; anyone else gets zero rows.
UPDATE jobs SET lease_until = now() + interval '30 seconds'
 WHERE id = $1 AND worker = $2 AND state = 'leased';
```"""

BASH = """```bash
# One page of queue triage: who is running, who is waiting, who is retrying.
psql -c "SELECT state, count(*), max(attempts) FROM jobs GROUP BY 1 ORDER BY 2 DESC"
psql -c "SELECT key, count(*) FROM jobs WHERE state = 'ready' GROUP BY 1 ORDER BY 2 DESC LIMIT 10"
psql -c "SELECT count(*) AS expired FROM jobs WHERE state = 'leased' AND lease_until < now()"
psql -c "SELECT wait_event_type, count(*) FROM pg_stat_activity GROUP BY 1"   # who blocks
```"""

GO = """```go
// Per-key concurrency cap: parallelism comes from keys, not from hope.
func (l *Limiter) Acquire(ctx context.Context, key string) (release func(), err error) {
	for {
		l.mu.Lock()
		if l.inflight[key] < l.perKey {
			l.inflight[key]++
			l.mu.Unlock()
			return func() { l.mu.Lock(); l.inflight[key]--; l.mu.Unlock() }, nil
		}
		wait := l.notify(key) // 返回一个 channel，释放时会 close
		l.mu.Unlock()
		select {
		case <-wait:
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
}
```"""

TABLES = [
    """| State | Meaning | Allowed transitions |
|---|---|---|
| `ready` | waiting for a dispatcher | `leased`, `succeeded`, `failed` |
| `leased` | a worker holds a deadline | `ready` (expiry), `succeeded`, `failed` |
| `succeeded` | terminal, side effect confirmed | — |
| `failed` | terminal, budget exhausted | `ready` (manual requeue) |""",
    """| Parameter | Default | Why this value |
|---|---|---|
| lease TTL | 30s | 2× the p99 of the slowest handler we accept |
| renew at | 15s | half the TTL: one lost renewal is survivable |
| claim batch | 16 | large enough to amortize, small enough to rebalance |
| sweep interval | 5s ± 1s | jitter keeps replicas from sweeping in lockstep |
| max attempts | 5 | 1 + 4 retries ≈ 4s / 8s / 16s / 30s with jitter |""",
]

QUOTES = [
    """> The queue is not a place where work waits; it is a contract about when the
> system will admit that it cannot do the work right now.""",
    """> If you cannot describe a job's terminal states, you cannot describe its
> retries either — and you will find out during an incident.""",
]

MATH = [
    """The expected wait of a job at position $k$ in a fair queue with mean service
time $\\bar{s}$ is $W_k \\approx k \\cdot \\bar{s}$, and the tail grows with the
variance of the service distribution:

$$P(W > t) \\le \\exp\\left(-\\frac{t}{k \\bar{s}} \\cdot \\frac{2\\mu^2}{\\sigma^2 + \\mu^2}\\right)$$

which is why we cap the *variance* — long handlers get their own queue.""",
]

PROSE_TAIL = [
    """We instrument exactly three numbers per key: depth, lease-expiry rate, and the
p95 of `claim → first byte`. Depth tells demand, expiry tells bugs, and the
`claim → first byte` latency tells you whether the dispatcher itself became the
bottleneck (it does, at around 40k claims/s on a single primary).""",
    """The last rewrite removed 1,900 lines. It turned out most of the complexity came
from trying to make the broker the source of truth for liveness; once the lease
table owned that question, the broker went back to being a transport.""",
    """Sizing rule of thumb: one dispatcher per 8 workers, workers sized to the p99 of
the handler rather than the mean, and a queue depth alert at 10× the number of
workers. Beyond that you are not queueing work, you are hiding an outage.""",
]


def build_corpus(target_bytes: int) -> str:
    """确定性生成 ``target_bytes`` 量级的 markdown（同一 seed ⇒ 同一份文本）。"""
    rng = random.Random(20261004)
    parts: list[str] = [TITLE]
    blocks = [
        PARAGRAPHS,
        BULLETS,
        [RUST, TYPESCRIPT, PYTHON, SQL, BASH, GO],
        TABLES,
        QUOTES,
        MATH,
    ]
    section = 0
    size = len(TITLE)
    while size < target_bytes:
        section += 1
        chunk: list[str] = [f"\n\n## {section}. {_heading(rng)}"]

        for _ in range(rng.randint(2, 3)):
            chunk.append("\n\n" + rng.choice(PARAGRAPHS).strip())
        chunk.append("\n\n" + rng.choice(BULLETS).strip())
        for group in blocks[2:3] + [TABLES, QUOTES, MATH]:
            chunk.append("\n\n" + rng.choice(group).strip())
        chunk.append("\n\n" + rng.choice(PROSE_TAIL).strip())

        text = "".join(chunk)
        parts.append(text)
        size += len(text)

    body = "".join(parts)
    return body[:target_bytes] if len(body) > target_bytes else body


HEADINGS = [
    "The claim path",
    "Leases and deadlines",
    "Retry budgets",
    "Fairness per key",
    "The sweeper",
    "Backpressure at the edge",
    "Idempotency at the boundary",
    "Observability we kept",
    "What we deleted",
    "Sizing and capacity",
]


def _heading(rng: random.Random) -> str:
    return rng.choice(HEADINGS)


def main() -> int:
    parser = argparse.ArgumentParser(description="generate the demo corpus")
    parser.add_argument("--bytes", type=int, default=260_000)
    args = parser.parse_args()
    corpus = build_corpus(args.bytes)
    print(corpus, end="")
    print(
        f"\n[corpus] {len(corpus)} bytes (target {args.bytes})",
        file=__import__("sys").stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
