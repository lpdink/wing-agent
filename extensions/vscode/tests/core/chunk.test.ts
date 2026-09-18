import { describe, expect, it } from 'vitest';

import {
  CHUNK_IDLE_TIMEOUT_MS,
  CHUNK_TYPE,
  ChunkReassembler,
  MAX_CHUNKS,
  type ChunkOutcome,
  parseChunkEnvelope,
} from '../../src/core/chunk';
import { isKnownEvent } from '../../src/core/protocol/events';
import { chunkEnvelope, chunkFrames, syncSessionPayload, textPayload } from './support/fake-gateway';

/**
 * Chunk reassembly — the transport-layer contract ported from
 * `crates/wing/src/gateway/chunk.rs`.
 *
 * Every case below mirrors a Rust test of the same name; the ones that matter
 * for the "timing bug" class are the ordering test (frames behind an open window
 * must not overtake the reassembled event) and the bounded/safe-fail cases.
 */

function deliver(outcome: ChunkOutcome): readonly unknown[] {
  if (outcome.kind !== 'deliver') {
    throw new Error(`expected deliver, got ${outcome.kind}`);
  }
  return outcome.events;
}

function fail(outcome: ChunkOutcome): string {
  if (outcome.kind !== 'fail') {
    throw new Error(`expected fail, got ${outcome.kind}`);
  }
  return outcome.detail;
}

function expectPending(outcome: ChunkOutcome): void {
  expect(outcome.kind).toBe('pending');
}

describe('chunk reassembly — pass-through', () => {
  it('delivers a plain event untouched', () => {
    const reassembler = new ChunkReassembler();
    const events = deliver(reassembler.onText(textPayload('hi')));
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ type: 'text', content: 'hi' });
    expect(reassembler.isAssembling()).toBe(false);
    expect(reassembler.deadline()).toBeNull();
  });

  it('delivers an unknown event as an unknown event (forward compatibility)', () => {
    const reassembler = new ChunkReassembler();
    const events = deliver(reassembler.onText(JSON.stringify({ type: 'from_the_future', x: 1 })));
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ type: 'from_the_future' });
    expect(isKnownEvent(events[0] as never)).toBe(false);
  });

  it('drops malformed JSON without delivering anything', () => {
    const reassembler = new ChunkReassembler();
    expect(deliver(reassembler.onText('{not json'))).toHaveLength(0);
    expect(reassembler.isAssembling()).toBe(false);
  });

  it('treats a broken _chunk envelope as an unknown event, not as a chunk', () => {
    const reassembler = new ChunkReassembler();
    // Missing `index` / `count` / `data`: not a usable envelope.
    const events = deliver(reassembler.onText(JSON.stringify({ type: CHUNK_TYPE, id: '1' })));
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ type: CHUNK_TYPE });
    expect(reassembler.isAssembling()).toBe(false);
  });
});

describe('chunk reassembly — happy path', () => {
  it('reassembles a two-frame payload into one complete event', () => {
    const payload = syncSessionPayload(64);
    const [first, last] = chunkFrames(payload, { id: 'c1' });
    const reassembler = new ChunkReassembler();

    expectPending(reassembler.onText(first as string));
    expect(reassembler.isAssembling()).toBe(true);
    expect(reassembler.deadline()).not.toBeNull();

    const events = deliver(reassembler.onText(last as string));
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ type: 'sync_session', session_id: 'sess-1' });
    expect(reassembler.isAssembling()).toBe(false);
  });

  it('survives a split inside a multi-byte character', () => {
    // The payload contains 4-byte emoji; slicing by code units cuts them apart,
    // exactly like the gateway's UTF-8 boundary splitting can look on the wire.
    const payload = syncSessionPayload(0, '会话🙂-session');
    const [first, last] = chunkFrames(payload, { id: 'c-mb' });
    const reassembler = new ChunkReassembler();

    expectPending(reassembler.onText(first as string));
    const events = deliver(reassembler.onText(last as string));
    expect(events[0]).toMatchObject({ type: 'sync_session', session_id: '会话🙂-session' });
  });

  it('closes only on the last of three frames', () => {
    const payload = syncSessionPayload(40);
    const frames = chunkFrames(payload, { id: 'c3', count: 3 });
    const reassembler = new ChunkReassembler();

    expectPending(reassembler.onText(frames[0] as string));
    expectPending(reassembler.onText(frames[1] as string));
    expect(deliver(reassembler.onText(frames[2] as string))).toHaveLength(1);
  });

  it('can start a new event after a completed one', () => {
    const payload = syncSessionPayload(16);
    const first = chunkFrames(payload, { id: 'c12' });
    const second = chunkFrames(payload, { id: 'c13' });
    const reassembler = new ChunkReassembler();

    expectPending(reassembler.onText(first[0] as string));
    expect(deliver(reassembler.onText(first[1] as string))).toHaveLength(1);
    expectPending(reassembler.onText(second[0] as string));
    expect(deliver(reassembler.onText(second[1] as string))).toHaveLength(1);
  });
});

describe('chunk reassembly — ordering invariant', () => {
  it('holds every frame back while the window is open, then releases in arrival order', () => {
    const payload = syncSessionPayload(32);
    const [first, last] = chunkFrames(payload, { id: 'c2' });
    const reassembler = new ChunkReassembler();

    expectPending(reassembler.onText(first as string));
    // Live deltas behind the sync payload must NOT overtake it, otherwise a host
    // that clears its transcript on replay would swallow them.
    expectPending(reassembler.onText(textPayload('live-1')));
    expectPending(reassembler.onText(textPayload('live-2')));

    const events = deliver(reassembler.onText(last as string));
    expect(events).toHaveLength(3);
    expect(events[0]).toMatchObject({ type: 'sync_session' });
    expect(events[1]).toMatchObject({ type: 'text', content: 'live-1' });
    expect(events[2]).toMatchObject({ type: 'text', content: 'live-2' });
  });

  it('drops an unparseable buffered frame and keeps the rest', () => {
    const payload = syncSessionPayload(32);
    const [first, last] = chunkFrames(payload, { id: 'c2b' });
    const reassembler = new ChunkReassembler();

    expectPending(reassembler.onText(first as string));
    expectPending(reassembler.onText('{ broken'));
    expectPending(reassembler.onText(textPayload('after')));

    const events = deliver(reassembler.onText(last as string));
    expect(events.map((event) => (event as { type: string }).type)).toStrictEqual(['sync_session', 'text']);
  });
});

describe('chunk reassembly — malformed envelopes fail safe', () => {
  it('requires the first frame to start at index 0', () => {
    const reassembler = new ChunkReassembler();
    const detail = fail(reassembler.onText(chunkEnvelope({ index: 1 })));
    expect(detail).toContain('expected 0');
    expect(reassembler.isAssembling()).toBe(false);
    expect(reassembler.deadline()).toBeNull();
  });

  it('rejects count < 2, count > MAX_CHUNKS and absurd counts without allocating', () => {
    for (const count of [1, MAX_CHUNKS + 1, 1_000_000_000]) {
      const reassembler = new ChunkReassembler();
      const detail = fail(reassembler.onText(chunkEnvelope({ count })));
      expect(detail).toContain(`count=${count}`);
      expect(reassembler.deadline()).toBeNull();
    }
  });

  it('rejects an index gap and a duplicated index', () => {
    const window = (): ChunkReassembler => {
      const reassembler = new ChunkReassembler();
      expectPending(reassembler.onText(chunkEnvelope({ index: 0, count: 3, data: '{"a":' })));
      return reassembler;
    };

    expect(fail(window().onText(chunkEnvelope({ index: 2, count: 3 })))).toContain('got 2, expected 1');
    expect(fail(window().onText(chunkEnvelope({ index: 0, count: 3 })))).toContain('got 0, expected 1');
  });

  it('rejects an id change and a count change mid-reassembly', () => {
    const window = (count = 2): ChunkReassembler => {
      const reassembler = new ChunkReassembler();
      expectPending(reassembler.onText(chunkEnvelope({ id: 'c7', index: 0, count, data: '[' })));
      return reassembler;
    };

    expect(fail(window().onText(chunkEnvelope({ id: 'c8', index: 1, count: 2, data: ']' })))).toContain(
      'id changed',
    );
    expect(fail(window(3).onText(chunkEnvelope({ id: 'c7', index: 1, count: 2, data: ']' })))).toContain(
      'count changed',
    );
  });

  it('rejects a reassembled payload that is not a JSON object event', () => {
    const reassembler = new ChunkReassembler();
    expectPending(
      reassembler.onText(chunkEnvelope({ id: 'c10', index: 0, count: 2, data: '{"type":"text"' })),
    );
    const detail = fail(reassembler.onText(chunkEnvelope({ id: 'c10', index: 1, count: 2, data: '[]' })));
    expect(detail).toContain('not a valid event');
    expect(reassembler.isAssembling()).toBe(false);
  });

  it('rejects a payload whose type is not a string', () => {
    const reassembler = new ChunkReassembler();
    expectPending(reassembler.onText(chunkEnvelope({ index: 0, data: '{"ty' })));
    expect(fail(reassembler.onText(chunkEnvelope({ index: 1, data: 'pe":1}' })))).toContain(
      'not a valid event',
    );
  });
});

describe('chunk reassembly — bounds', () => {
  it('rejects a first fragment that already exceeds the byte cap', () => {
    const reassembler = new ChunkReassembler({ limits: { maxBufferedBytes: 64 } });
    const detail = fail(reassembler.onText(chunkEnvelope({ data: 'x'.repeat(65) })));
    expect(detail).toContain('buffer exceeded');
  });

  it('counts fragments plus buffered frames against the same cap', () => {
    const reassembler = new ChunkReassembler({ limits: { maxBufferedBytes: 200 } });
    expectPending(reassembler.onText(chunkEnvelope({ index: 0, count: 2, data: 'x'.repeat(40) })));
    expectPending(reassembler.onText(JSON.stringify({ type: 'text', content: 'y'.repeat(60) })));
    const detail = fail(reassembler.onText(JSON.stringify({ type: 'text', content: 'z'.repeat(60) })));
    expect(detail).toContain('buffer exceeded');
  });

  it('measures bytes as UTF-8, not as JS string length', () => {
    // 30 CJK characters = 90 UTF-8 bytes but only 30 code units.
    const cjk = '汉'.repeat(30);
    expect(cjk.length).toBe(30);
    const reassembler = new ChunkReassembler({ limits: { maxBufferedBytes: 80 } });
    const detail = fail(reassembler.onText(chunkEnvelope({ data: cjk })));
    expect(detail).toContain('90 > 80');
  });

  it('caps how many frames one event may declare', () => {
    const reassembler = new ChunkReassembler({ limits: { maxChunks: 3 } });
    expect(fail(reassembler.onText(chunkEnvelope({ count: 4 })))).toContain('allowed 2..=3');
  });
});

describe('chunk reassembly — deadline', () => {
  it('arms, renews and clears the deadline', () => {
    let now = 1_000;
    const reassembler = new ChunkReassembler({ now: () => now });
    const frames = chunkFrames(syncSessionPayload(16), { id: 'c-deadline', count: 3 });

    expectPending(reassembler.onText(frames[0] as string));
    expect(reassembler.deadline()).toBe(1_000 + CHUNK_IDLE_TIMEOUT_MS);

    now += 5_000;
    expectPending(reassembler.onText(frames[1] as string));
    expect(reassembler.deadline()).toBe(6_000 + CHUNK_IDLE_TIMEOUT_MS);

    expect(deliver(reassembler.onText(frames[2] as string))).toHaveLength(1);
    expect(reassembler.deadline()).toBeNull();
  });

  it('honours a custom idle timeout in the timeout detail', () => {
    const reassembler = new ChunkReassembler({ limits: { idleTimeoutMs: 4_000 } });
    expect(reassembler.timeoutDetail()).toBe('chunk reassembly idle timeout');

    expectPending(reassembler.onText(chunkEnvelope({ index: 0, count: 3, data: 'a' })));
    const detail = reassembler.timeoutDetail();
    expect(detail).toContain('1 of 3');
    expect(detail).toContain('4s');
  });

  it('reset() drops an open window', () => {
    const reassembler = new ChunkReassembler();
    expectPending(reassembler.onText(chunkEnvelope({ index: 0, count: 2, data: 'a' })));
    reassembler.reset();
    expect(reassembler.isAssembling()).toBe(false);
    expect(reassembler.deadline()).toBeNull();
    expect(deliver(reassembler.onText(textPayload('after reset')))).toHaveLength(1);
  });
});

describe('parseChunkEnvelope', () => {
  it('accepts a well-formed envelope and rejects everything else', () => {
    expect(parseChunkEnvelope(JSON.parse(chunkEnvelope({ id: 'c14' })))).toStrictEqual({
      id: 'c14',
      index: 0,
      count: 2,
      of_type: 'sync_session',
      data: '{}',
    });
    expect(parseChunkEnvelope(JSON.parse(textPayload('hi')))).toBeNull();
    expect(parseChunkEnvelope({})).toBeNull();
    expect(parseChunkEnvelope(null)).toBeNull();
    // Non-integer / negative indices are not envelopes (Rust's `usize` behaves the same).
    expect(
      parseChunkEnvelope({ type: CHUNK_TYPE, id: 'a', index: -1, count: 2, of_type: 'x', data: '' }),
    ).toBeNull();
    expect(
      parseChunkEnvelope({ type: CHUNK_TYPE, id: 'a', index: 1.5, count: 2, of_type: 'x', data: '' }),
    ).toBeNull();
  });
});
