/**
 * Chunked-frame reassembly — the transport-layer half of the `_chunk` contract.
 *
 * Background (`libs/core/wing/gateway/frames.py`): a client's per-frame limit is
 * 16 MiB while a `sync_session` payload can be far larger (22 MB measured). The
 * gateway therefore splits oversized payloads at UTF-8 boundaries into N
 * envelope frames (`type == "_chunk"`), and the **reading** side merges them
 * back before anything reaches the application. Application code only ever sees
 * complete events.
 *
 * This is a port of `crates/wing/src/gateway/chunk.rs`; the three invariants are
 * the same:
 *
 * 1. **Ordering** — while a window is open (first fragment seen, last not yet),
 *    every other frame is buffered raw without being parsed or delivered; when
 *    the window closes, the reassembled event is delivered first and the
 *    buffered frames follow in arrival order. Otherwise a `sync_session` could
 *    interleave with the live deltas behind it and a host that clears its chat
 *    on replay would swallow them.
 * 2. **Bounded** — frame count cap, buffered byte cap, idle deadline; exceeding
 *    any of them fails the window (the caller closes the socket and lets the
 *    reconnect path resynchronise).
 * 3. **Fail safe** — a malformed envelope never produces a corrupt event, and
 *    `count` is never used to pre-allocate.
 *
 * The state machine is pure (no I/O, no timers): `onText` gets one raw frame and
 * answers deliver / pending / fail, and the caller owns the clock via
 * {@link ChunkReassembler.deadline}.
 */

import { type WingEvent, decodeWingEvent, isKnownEvent, isKnownEventType } from './protocol/events';
import { isJsonObject, tryParseJson } from './protocol/json';
import { type CoreLogger, silentLogger } from './logging';
import { utf8ByteLength } from './text';

/** Envelope `type`; `_`-prefixed types are reserved for the transport layer. */
export const CHUNK_TYPE = '_chunk';

/** Frame-count ceiling for one event (defends against a bogus `count`). */
export const MAX_CHUNKS = 1024;

/** Byte ceiling for an open window (fragments + frames buffered behind it). */
export const MAX_BUFFERED_BYTES = 64 * 1024 * 1024;

/** Idle deadline: no new fragment for this long fails the window. */
export const CHUNK_IDLE_TIMEOUT_MS = 30_000;

/** Injectable limits (production uses the defaults; tests use tiny values). */
export interface ChunkLimits {
  readonly maxChunks: number;
  readonly maxBufferedBytes: number;
  readonly idleTimeoutMs: number;
}

export const DEFAULT_CHUNK_LIMITS: ChunkLimits = {
  maxChunks: MAX_CHUNKS,
  maxBufferedBytes: MAX_BUFFERED_BYTES,
  idleTimeoutMs: CHUNK_IDLE_TIMEOUT_MS,
};

/** One `_chunk` envelope (see the table in `docs/dev/http-api.md`). */
export interface ChunkEnvelope {
  /** Shared by every frame of one event; unique among events. */
  readonly id: string;
  /** 0-based frame index. */
  readonly index: number;
  /** Total frame count of the event (≥ 2). */
  readonly count: number;
  /** `type` of the original event (diagnostics only). */
  readonly of_type: string;
  /** A slice of the original payload JSON text. */
  readonly data: string;
}

/** Result of feeding one raw frame. */
export type ChunkOutcome =
  /** Zero or more complete events, in delivery order. */
  | { readonly kind: 'deliver'; readonly events: readonly WingEvent[] }
  /** A window is open; this frame produced nothing yet. */
  | { readonly kind: 'pending' }
  /** Protocol violation: the caller must close the connection and reconnect. */
  | { readonly kind: 'fail'; readonly detail: string };

interface OpenWindow {
  readonly id: string;
  readonly of_type: string;
  readonly count: number;
  nextIndex: number;
  readonly parts: string[];
  bytes: number;
  deadlineMs: number;
}

export interface ChunkReassemblerOptions {
  readonly limits?: Partial<ChunkLimits>;
  /** Injectable clock for {@link ChunkReassembler.deadline} (tests). */
  readonly now?: () => number;
  readonly logger?: CoreLogger;
}

function isIndex(value: unknown): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0;
}

/**
 * Parse a `_chunk` envelope; `null` for anything else (including a `_chunk`
 * frame with missing / ill-typed fields, which is treated as an unknown event).
 */
export function parseChunkEnvelope(value: unknown): ChunkEnvelope | null {
  if (!isJsonObject(value) || value['type'] !== CHUNK_TYPE) {
    return null;
  }
  const id = value['id'];
  const index = value['index'];
  const count = value['count'];
  const ofType = value['of_type'];
  const data = value['data'];
  if (typeof id !== 'string' || typeof ofType !== 'string' || typeof data !== 'string') {
    return null;
  }
  if (!isIndex(index) || !isIndex(count)) {
    return null;
  }
  return { id, index, count, of_type: ofType, data };
}

export class ChunkReassembler {
  private readonly limits: ChunkLimits;
  private readonly now: () => number;
  private readonly logger: CoreLogger;
  private pending: OpenWindow | null = null;
  private buffered: string[] = [];
  private bufferedBytes = 0;

  constructor(options: ChunkReassemblerOptions = {}) {
    this.limits = { ...DEFAULT_CHUNK_LIMITS, ...options.limits };
    this.now = options.now ?? Date.now;
    this.logger = options.logger ?? silentLogger;
  }

  /** `true` while a fragmented event is incomplete. */
  isAssembling(): boolean {
    return this.pending !== null;
  }

  /** Absolute ms timestamp by which the next fragment must arrive, or `null`. */
  deadline(): number | null {
    return this.pending?.deadlineMs ?? null;
  }

  /** Human-readable reason for the caller's timeout branch. */
  timeoutDetail(): string {
    const pending = this.pending;
    if (pending === null) {
      return 'chunk reassembly idle timeout';
    }
    return (
      `chunked event "${pending.of_type}" (id=${pending.id}) not completed: ` +
      `${pending.nextIndex} of ${pending.count} frames, ` +
      `no new frame for ${Math.round(this.limits.idleTimeoutMs / 1000)}s`
    );
  }

  /** Drop any open window (called when a new connection starts). */
  reset(): void {
    this.pending = null;
    this.buffered = [];
    this.bufferedBytes = 0;
  }

  /** Feed one raw text frame. */
  onText(text: string): ChunkOutcome {
    if (this.pending !== null) {
      const windowed = tryParseJson(text);
      const envelope = windowed.ok ? parseChunkEnvelope(windowed.value) : null;
      if (envelope !== null) {
        return this.acceptChunk(envelope);
      }
      // Ordinary frame behind an open window: keep it raw, release it later.
      // (Raw, not parsed: garbage counts against the buffer cap and is dropped
      // when the window closes — same as `chunk.rs`.)
      return this.buffer(text);
    }

    const result = tryParseJson(text);
    if (!result.ok) {
      this.logger.warn('dropping frame that is not valid JSON', truncate(text));
      return { kind: 'deliver', events: [] };
    }
    const parsed = result.value;

    const event = decodeWingEvent(parsed);
    if (!isKnownEvent(event)) {
      // Unknown types are the entry signal for chunking; a broken `_chunk`
      // envelope is *not* a chunk and is delivered as unknown, like every other
      // future type (`chunk.rs` behaves the same way).
      const envelope = parseChunkEnvelope(parsed);
      if (envelope !== null) {
        return this.start(envelope);
      }
    }
    return { kind: 'deliver', events: [event] };
  }

  /** First fragment: open the window. */
  private start(envelope: ChunkEnvelope): ChunkOutcome {
    if (envelope.count < 2 || envelope.count > this.limits.maxChunks) {
      return {
        kind: 'fail',
        detail: `chunked event "${envelope.of_type}" (id=${envelope.id}) declares count=${envelope.count} (allowed 2..=${this.limits.maxChunks})`,
      };
    }
    if (envelope.index !== 0) {
      return {
        kind: 'fail',
        detail: `chunked event "${envelope.of_type}" (id=${envelope.id}) starts at index ${envelope.index} (expected 0)`,
      };
    }
    const bytes = utf8ByteLength(envelope.data);
    if (bytes > this.limits.maxBufferedBytes) {
      return {
        kind: 'fail',
        detail: `chunk reassembly buffer exceeded: ${bytes} > ${this.limits.maxBufferedBytes} bytes`,
      };
    }
    this.pending = {
      id: envelope.id,
      of_type: envelope.of_type,
      count: envelope.count,
      nextIndex: 1,
      parts: [envelope.data],
      bytes,
      deadlineMs: this.now() + this.limits.idleTimeoutMs,
    };
    return { kind: 'pending' };
  }

  /** Continuation: validate contiguity, accumulate, close on the last frame. */
  private acceptChunk(envelope: ChunkEnvelope): ChunkOutcome {
    const pending = this.pending;
    if (pending === null) {
      // Only reachable through a programming error; fail safe.
      return { kind: 'fail', detail: 'chunk fragment received without an open window' };
    }
    if (envelope.id !== pending.id) {
      return {
        kind: 'fail',
        detail: `chunk id changed mid-reassembly: "${pending.id}" -> "${envelope.id}"`,
      };
    }
    if (envelope.count !== pending.count) {
      return {
        kind: 'fail',
        detail: `chunk count changed mid-reassembly (id=${pending.id}): ${pending.count} -> ${envelope.count}`,
      };
    }
    if (envelope.index !== pending.nextIndex) {
      return {
        kind: 'fail',
        detail: `chunk index out of sequence (id=${pending.id}): got ${envelope.index}, expected ${pending.nextIndex}`,
      };
    }

    const bytes = utf8ByteLength(envelope.data);
    const total = pending.bytes + this.bufferedBytes + bytes;
    if (total > this.limits.maxBufferedBytes) {
      return {
        kind: 'fail',
        detail: `chunk reassembly buffer exceeded: ${total} > ${this.limits.maxBufferedBytes} bytes`,
      };
    }

    pending.bytes += bytes;
    pending.nextIndex += 1;
    pending.parts.push(envelope.data);
    pending.deadlineMs = this.now() + this.limits.idleTimeoutMs;

    if (pending.nextIndex < pending.count) {
      return { kind: 'pending' };
    }

    this.pending = null;
    const joined = pending.parts.join('');
    const result = tryParseJson(joined);
    const parsed: unknown = result.ok ? result.value : undefined;
    // Closure criterion, mirroring `chunk.rs` (`serde_json::from_str::<WingEvent>`):
    // the joined payload must be a *well-formed* event. A declared type this build
    // knows but cannot decode (missing / ill-typed required field) means the
    // reassembled payload is corrupt — usually a broken `sync_session` replay —
    // and the correct recovery is to drop the connection so the reconnect path
    // resynchronises, not to hand the host a replay it silently cannot apply.
    // Unknown types stay deliverable (forward compatibility): `chunk.rs` has the
    // same escape hatch through `WingEvent::Unknown`.
    const malformed =
      !isJsonObject(parsed) ||
      typeof parsed['type'] !== 'string' ||
      (isKnownEventType(parsed['type']) && !isKnownEvent(decodeWingEvent(parsed)));
    if (malformed) {
      return {
        kind: 'fail',
        detail: `reassembled "${pending.of_type}" payload (id=${pending.id}) is not a valid event`,
      };
    }
    return { kind: 'deliver', events: [decodeWingEvent(parsed), ...this.flush()] };
  }

  /** Buffer a frame that arrived behind an open window. */
  private buffer(text: string): ChunkOutcome {
    const bytes = utf8ByteLength(text);
    const pendingBytes = this.pending?.bytes ?? 0;
    const total = pendingBytes + this.bufferedBytes + bytes;
    if (total > this.limits.maxBufferedBytes) {
      return {
        kind: 'fail',
        detail: `chunk reassembly buffer exceeded: ${total} > ${this.limits.maxBufferedBytes} bytes`,
      };
    }
    this.bufferedBytes += bytes;
    this.buffered.push(text);
    return { kind: 'pending' };
  }

  /** Release buffered frames in arrival order (unparseable ones are dropped). */
  private flush(): WingEvent[] {
    const events: WingEvent[] = [];
    while (this.buffered.length > 0) {
      const text = this.buffered.shift();
      if (text === undefined) {
        break;
      }
      this.bufferedBytes = Math.max(0, this.bufferedBytes - utf8ByteLength(text));
      const result = tryParseJson(text);
      if (!result.ok) {
        this.logger.warn('dropping buffered frame that is not valid JSON', truncate(text));
        continue;
      }
      events.push(decodeWingEvent(result.value));
    }
    return events;
  }
}

/** Keep log lines bounded — a failed frame can be megabytes. */
function truncate(text: string, max = 200): string {
  return text.length <= max ? text : `${text.slice(0, max)}… (${text.length} chars)`;
}
