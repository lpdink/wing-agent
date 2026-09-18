import { describe, expect, it } from 'vitest';

import {
  type KnownWingEvent,
  type SyncSessionEvent,
  type WingEvent,
  decodeWingEvent,
  isKnownEvent,
  isKnownEventType,
  KNOWN_EVENT_TYPES,
} from '../../src/core/protocol/events';

/**
 * Protocol mirror tests — one payload per event type, asserted **field by field**
 * against `libs/core/wing/event/**.py`.
 *
 * `toStrictEqual` is deliberate: it fails for a missing *and* for an extra field,
 * so the expected object below is a complete statement of the mirror. The
 * registry test at the bottom guards the other direction (a variant that exists
 * in Python but not here).
 */

const META = { created_at: '2026-09-18T08:00:00.123456', request_id: 'req-77' };

/** Decode a payload and require a known event (fails loudly otherwise). */
function known(payload: unknown): KnownWingEvent {
  const event = decodeWingEvent(payload);
  if (!isKnownEvent(event)) {
    throw new Error(`expected a known event, got unknown type "${event.type}"`);
  }
  return event;
}

/** Decode a payload that must be a `sync_session`. */
function syncSession(payload: unknown): SyncSessionEvent {
  const event = known(payload);
  if (event.type !== 'sync_session') {
    throw new Error(`expected sync_session, got ${event.type}`);
  }
  return event;
}

describe('event mirror — every type decodes field by field', () => {
  it('error (defaults filled, nullable fields null)', () => {
    expect(known({ type: 'error', message: 'boom', ...META })).toStrictEqual({
      ...META,
      type: 'error',
      status_code: 500,
      message: 'boom',
      error_code: null,
      detail: null,
      session_id: null,
      uuid: null,
    });
  });

  it('error carries status / code / detail when present', () => {
    const event = known({
      type: 'error',
      status_code: 404,
      message: 'session not found',
      error_code: 'not_found',
      detail: 'no such session',
      session_id: 'sess-1',
      ...META,
    });
    expect(event).toMatchObject({
      status_code: 404,
      error_code: 'not_found',
      detail: 'no such session',
      session_id: 'sess-1',
    });
  });

  it('delivered', () => {
    expect(known({ type: 'delivered', session_id: 'sess-1', ...META })).toStrictEqual({
      ...META,
      type: 'delivered',
      session_id: 'sess-1',
      uuid: null,
    });
  });

  it('notice (retry progress)', () => {
    expect(
      known({
        type: 'notice',
        level: 'warning',
        message: 'LLM call failed, retrying',
        attempt: 2,
        max_attempts: 5,
        retry_in_s: 1.5,
        session_id: 'sess-1',
        ...META,
      }),
    ).toStrictEqual({
      ...META,
      type: 'notice',
      level: 'warning',
      message: 'LLM call failed, retrying',
      attempt: 2,
      max_attempts: 5,
      retry_in_s: 1.5,
      session_id: 'sess-1',
      uuid: null,
    });
  });

  it('notice degrades an unknown level to info (matching the Rust mirror)', () => {
    const event = known({ type: 'notice', level: 'catastrophic', message: 'hi', ...META });
    expect(event).toMatchObject({ level: 'info' });
  });

  it('text / reasoning', () => {
    expect(known({ type: 'text', content: 'hello', session_id: 's', ...META })).toStrictEqual({
      ...META,
      type: 'text',
      content: 'hello',
      session_id: 's',
      uuid: null,
    });
    expect(known({ type: 'reasoning', content: 'thinking', ...META })).toMatchObject({
      type: 'reasoning',
      content: 'thinking',
    });
  });

  it('tool_call (authoritative parsed args)', () => {
    expect(
      known({
        type: 'tool_call',
        tool_name: 'Bash',
        tool_args: { command: 'ls', nested: { deep: [1, 2, 3] } },
        tool_call_id: 'call-1',
        session_id: 's',
        ...META,
      }),
    ).toStrictEqual({
      ...META,
      type: 'tool_call',
      tool_name: 'Bash',
      tool_args: { command: 'ls', nested: { deep: [1, 2, 3] } },
      tool_call_id: 'call-1',
      session_id: 's',
      uuid: null,
    });
  });

  it('tool_call_stream (raw args fragment)', () => {
    expect(
      known({
        type: 'tool_call_stream',
        tool_call_id: 'call-1',
        tool_name: 'Write',
        args_fragment: '{"path": "a',
        is_final: false,
        ...META,
      }),
    ).toStrictEqual({
      ...META,
      type: 'tool_call_stream',
      tool_call_id: 'call-1',
      tool_name: 'Write',
      args_fragment: '{"path": "a',
      is_final: false,
      session_id: null,
      uuid: null,
    });
  });

  it('tool_call_result', () => {
    expect(
      known({
        type: 'tool_call_result',
        tool_name: 'Bash',
        tool_args: { command: 'ls' },
        tool_call_id: 'call-1',
        tool_result: 'file.txt',
        tool_success: true,
        model: 'gpt-5',
        ...META,
      }),
    ).toMatchObject({
      type: 'tool_call_result',
      tool_result: 'file.txt',
      tool_success: true,
      model: 'gpt-5',
    });
  });

  it('llm_call_metrics (nullable stop_reason)', () => {
    expect(
      known({
        type: 'llm_call_metrics',
        model: 'claude-sonnet-4',
        prompt_tokens: 1200,
        completion_tokens: 340,
        cached_tokens: 800,
        first_chunk_rt_ms: 412.5,
        tokens_per_sec: 61.2,
        ...META,
      }),
    ).toStrictEqual({
      ...META,
      type: 'llm_call_metrics',
      model: 'claude-sonnet-4',
      prompt_tokens: 1200,
      completion_tokens: 340,
      cached_tokens: 800,
      first_chunk_rt_ms: 412.5,
      tokens_per_sec: 61.2,
      stop_reason: null,
      session_id: null,
      uuid: null,
    });
  });

  it('ask — multi-question shape (AskUserQuestion)', () => {
    expect(
      known({
        type: 'ask',
        tool_call_id: 'call-9',
        questions: [
          {
            id: 'q1',
            header: 'Storage',
            question: 'Where should the data live?',
            multiSelect: false,
            options: [{ label: 'file', description: 'durable' }, { label: 'memory' }],
          },
        ],
        session_id: 's',
        ...META,
      }),
    ).toStrictEqual({
      ...META,
      type: 'ask',
      tool_call_id: 'call-9',
      questions: [
        {
          id: 'q1',
          header: 'Storage',
          question: 'Where should the data live?',
          multiSelect: false,
          options: [
            { label: 'file', description: 'durable' },
            { label: 'memory', description: '' },
          ],
          choices: [],
        },
      ],
      question: '',
      choices: [],
      required: false,
      session_id: 's',
      uuid: null,
    });
  });

  it('ask — legacy single-question shape (Bash approval)', () => {
    expect(
      known({
        type: 'ask',
        question: 'Run a dangerous command?',
        choices: ['Yes', 'No'],
        required: true,
        session_id: 's',
        ...META,
      }),
    ).toMatchObject({
      type: 'ask',
      tool_call_id: '',
      questions: [],
      question: 'Run a dangerous command?',
      choices: ['Yes', 'No'],
      required: true,
    });
  });

  it('done / turn_started / interrupted are meta-only', () => {
    for (const type of ['done', 'turn_started', 'interrupted'] as const) {
      expect(known({ type, session_id: 's', ...META })).toStrictEqual({
        ...META,
        type,
        session_id: 's',
        uuid: null,
      });
    }
  });

  it('user_message_accepted', () => {
    expect(
      known({
        type: 'user_message_accepted',
        content: 'do it',
        origin_request_id: 'req-9',
        session_id: 's',
        ...META,
      }),
    ).toMatchObject({ content: 'do it', origin_request_id: 'req-9' });
  });

  it('diff_content (window with absolute start lines)', () => {
    expect(
      known({
        type: 'diff_content',
        path: '/tmp/a.ts',
        old_text: 'a\nb',
        new_text: 'a\nc',
        old_start_line: 12,
        new_start_line: 12,
        tool_call_id: 'call-2',
        session_id: 's',
        ...META,
      }),
    ).toStrictEqual({
      ...META,
      type: 'diff_content',
      path: '/tmp/a.ts',
      old_text: 'a\nb',
      new_text: 'a\nc',
      old_start_line: 12,
      new_start_line: 12,
      tool_call_id: 'call-2',
      session_id: 's',
      uuid: null,
    });
  });

  it('diff_content defaults the start lines to 1 (pre-windowing payloads)', () => {
    const event = known({ type: 'diff_content', path: '/tmp/new.ts', new_text: 'x', ...META });
    expect(event).toMatchObject({ old_text: null, old_start_line: 1, new_start_line: 1 });
  });

  it('assistant_turn (content blocks + usage)', () => {
    expect(
      known({
        type: 'assistant_turn',
        uuid: 'turn-1',
        content_blocks: [
          { type: 'thinking', thinking: 'hmm' },
          { type: 'text', text: 'answer' },
          { type: 'tool_use', id: 'call-1', name: 'Bash', input: { command: 'ls' } },
        ],
        model: 'gpt-5',
        stop_reason: 'tool_use',
        usage: { input_tokens: 10, output_tokens: 2, cached_tokens: 0 },
        session_id: 's',
        ...META,
      }),
    ).toStrictEqual({
      ...META,
      type: 'assistant_turn',
      uuid: 'turn-1',
      content_blocks: [
        { type: 'thinking', thinking: 'hmm' },
        { type: 'text', text: 'answer' },
        { type: 'tool_use', id: 'call-1', name: 'Bash', input: { command: 'ls' } },
      ],
      model: 'gpt-5',
      stop_reason: 'tool_use',
      usage: { input_tokens: 10, output_tokens: 2, cached_tokens: 0 },
      session_id: 's',
    });
  });

  it('tool_result_turn', () => {
    expect(
      known({
        type: 'tool_result_turn',
        uuid: 'tr-1',
        tool_use_id: 'call-1',
        tool_name: 'Bash',
        content: 'ok',
        is_error: true,
        session_id: 's',
        ...META,
      }),
    ).toMatchObject({ tool_use_id: 'call-1', content: 'ok', is_error: true });
  });

  it('turn_result (result / usage nullable, errors list)', () => {
    expect(
      known({
        type: 'turn_result',
        uuid: 'res-1',
        subtype: 'error_max_turns',
        is_error: true,
        num_turns: 12,
        duration_ms: 3456,
        errors: ['too many turns'],
        session_id: 's',
        ...META,
      }),
    ).toStrictEqual({
      ...META,
      type: 'turn_result',
      uuid: 'res-1',
      subtype: 'error_max_turns',
      is_error: true,
      result: null,
      num_turns: 12,
      duration_ms: 3456,
      usage: null,
      errors: ['too many turns'],
      session_id: 's',
    });
  });

  it('session_init', () => {
    expect(
      known({
        type: 'session_init',
        uuid: 'init-1',
        tools: ['Bash', 'Read'],
        model: 'gpt-5',
        permission_mode: 'default',
        cwd: '/tmp/ws',
        session_id: 's',
        ...META,
      }),
    ).toMatchObject({ tools: ['Bash', 'Read'], cwd: '/tmp/ws' });
  });

  it('compact_done', () => {
    expect(
      known({
        type: 'compact_done',
        original_tokens: 90_000,
        compressed_tokens: 12_000,
        model: 'gpt-5',
        session_id: 's',
        ...META,
      }),
    ).toMatchObject({ original_tokens: 90_000, compressed_tokens: 12_000, model: 'gpt-5' });
  });

  it('session_state_changed (only the changed fields travel)', () => {
    expect(known({ type: 'session_state_changed', thinking: true, session_id: 's', ...META })).toStrictEqual({
      ...META,
      type: 'session_state_changed',
      model: null,
      thinking: true,
      reasoning_effort: null,
      yolo: null,
      title: null,
      agent: null,
      session_id: 's',
      uuid: null,
    });
  });

  it('context_stats', () => {
    expect(
      known({
        type: 'context_stats',
        message_count: 42,
        total_tokens: 12_345,
        context_window_tokens: 200_000,
        system_prompt_parts: ['base'],
        session_id: 's',
        ...META,
      }),
    ).toMatchObject({
      message_count: 42,
      total_tokens: 12_345,
      context_window_tokens: 200_000,
      system_prompt_parts: ['base'],
    });
  });

  it('branch_targets', () => {
    expect(
      known({
        type: 'branch_targets',
        targets: [
          { uuid: 'u1', content: 'first prompt' },
          { uuid: 'u2', content: 'second', role: 'assistant' },
        ],
        session_id: 's',
        ...META,
      }),
    ).toMatchObject({
      targets: [
        { uuid: 'u1', content: 'first prompt', role: 'user' },
        { uuid: 'u2', content: 'second', role: 'assistant' },
      ],
    });
  });
});

describe('sync_session — the replay payload', () => {
  const payload = {
    type: 'sync_session',
    session_id: 'sess-9',
    messages: [
      { role: 'user', content: 'hello', uuid: 'm1' },
      {
        role: 'assistant',
        content: 'using a tool',
        uuid: 'm2',
        reasoning_content: 'because',
        tool_calls: [{ id: 'call-1', name: 'Bash', arguments: { command: 'ls' } }],
      },
      { role: 'tool', content: 'file.txt', uuid: 'm3', tool_call_id: 'call-1' },
    ],
    uncommitted: { role: 'assistant', content: 'partial', uuid: 'm4' },
    uncommitted_tools: [{ tool_call_id: 'call-2', tool_name: 'Write', args_fragment: '{"path"' }],
    events: [
      { type: 'interrupted', session_id: 'sess-9', ...META },
      { type: 'compact_done', original_tokens: 10, compressed_tokens: 5, session_id: 'sess-9', ...META },
    ],
    turn_started_at: '2026-09-18T07:59:00+00:00',
    agent: {
      model_name: 'gpt-5',
      system_prompt: 'be brief',
      tools: ['Bash'],
      skills: [],
      rules: [],
      workspace: '/tmp/ws',
      provider_name: 'openai',
    },
    name: 'My session',
    draft: 'unfinished draft',
    ...META,
  };

  it('decodes all four replay parts, in the documented order', () => {
    const event = syncSession(payload);
    expect(event).toMatchObject({
      session_id: 'sess-9',
      name: 'My session',
      draft: 'unfinished draft',
      turn_started_at: '2026-09-18T07:59:00+00:00',
    });
    expect(event.messages).toStrictEqual([
      {
        role: 'user',
        content: 'hello',
        uuid: 'm1',
        reasoning_content: null,
        tool_calls: [],
        tool_call_id: null,
      },
      {
        role: 'assistant',
        content: 'using a tool',
        uuid: 'm2',
        reasoning_content: 'because',
        tool_calls: [{ id: 'call-1', name: 'Bash', arguments: { command: 'ls' } }],
        tool_call_id: null,
      },
      {
        role: 'tool',
        content: 'file.txt',
        uuid: 'm3',
        reasoning_content: null,
        tool_calls: [],
        tool_call_id: 'call-1',
      },
    ]);
    expect(event.uncommitted).toMatchObject({ role: 'assistant', content: 'partial' });
    expect(event.uncommitted_tools).toStrictEqual([
      { tool_call_id: 'call-2', tool_name: 'Write', args_fragment: '{"path"' },
    ]);
    expect(event.agent).toStrictEqual({
      model_name: 'gpt-5',
      system_prompt: 'be brief',
      tools: ['Bash'],
      skills: [],
      rules: [],
      workspace: '/tmp/ws',
      provider_name: 'openai',
    });
  });

  it('turns replay facts into real events (same path as the live stream)', () => {
    const event = syncSession(payload);
    expect(event.events.map((inner) => inner.type)).toStrictEqual(['interrupted', 'compact_done']);
    const facts = event.events.filter(isKnownEvent);
    expect(facts[1]).toMatchObject({ type: 'compact_done', original_tokens: 10, compressed_tokens: 5 });
  });

  it('keeps an unknown replay fact as an unknown event (forward compatible)', () => {
    const event = syncSession({
      type: 'sync_session',
      session_id: 's',
      messages: [],
      events: [{ type: 'from_the_future', payload: 1, ...META }],
      ...META,
    });
    expect(event.events[0]).toMatchObject({ type: 'from_the_future' });
    expect(isKnownEvent(event.events[0] as WingEvent)).toBe(false);
  });

  it('skips undecodable history entries instead of failing the replay', () => {
    const event = syncSession({
      type: 'sync_session',
      session_id: 's',
      messages: ['not-a-message', { role: 'user', content: 'ok' }],
      ...META,
    });
    expect(event.messages).toHaveLength(1);
    expect(event.messages[0]).toMatchObject({ role: 'user', content: 'ok' });
  });

  it('requires session_id (a sync_session without one is not a sync_session)', () => {
    const event = decodeWingEvent({ type: 'sync_session', messages: [], ...META });
    expect(isKnownEvent(event)).toBe(false);
    expect(event).toMatchObject({ type: 'sync_session' });
  });
});

describe('unknown and malformed payloads never become typed events', () => {
  it('unknown type → UnknownWingEvent with the raw payload', () => {
    const event = decodeWingEvent({ type: 'future_event', session_id: 's', extra: { a: 1 } });
    expect(isKnownEvent(event)).toBe(false);
    expect(event).toStrictEqual({
      type: 'future_event',
      session_id: 's',
      raw: { type: 'future_event', session_id: 's', extra: { a: 1 } },
    });
  });

  it('known type with a missing required field → UnknownWingEvent', () => {
    const event = decodeWingEvent({ type: 'text', ...META });
    expect(isKnownEvent(event)).toBe(false);
    expect(event).toMatchObject({ type: 'text' });
  });

  it('known type with a wrongly-typed field → UnknownWingEvent', () => {
    const event = decodeWingEvent({ type: 'text', content: 42, ...META });
    expect(isKnownEvent(event)).toBe(false);
    expect(event).toMatchObject({ type: 'text' });
  });

  it('a chunk-like envelope is not an event', () => {
    const event = decodeWingEvent({
      type: '_chunk',
      id: '1',
      index: 0,
      count: 2,
      of_type: 'text',
      data: 'x',
    });
    expect(isKnownEvent(event)).toBe(false);
    expect(event).toMatchObject({ type: '_chunk' });
  });

  it('non-object payloads degrade to an empty unknown event', () => {
    expect(decodeWingEvent('nope')).toStrictEqual({ type: '', session_id: null, raw: {} });
    expect(decodeWingEvent(null)).toStrictEqual({ type: '', session_id: null, raw: {} });
    expect(decodeWingEvent([1, 2])).toStrictEqual({ type: '', session_id: null, raw: {} });
  });

  it('meta fields fall back to empty strings (old / hand-made payloads)', () => {
    const event = decodeWingEvent({ type: 'text', content: 'hi' });
    expect(event).toMatchObject({ created_at: '', request_id: '', session_id: null, uuid: null });
  });
});

describe('type registry', () => {
  /**
   * The literal key set of `EVENT_TYPES` in `libs/core/wing/event/__init__.py`.
   * Copied by hand on purpose: this test is the tripwire for a protocol change,
   * so it must fail when Python grows an event and the mirror does not.
   */
  const PYTHON_EVENT_TYPES = [
    'ask',
    'assistant_turn',
    'branch_targets',
    'compact_done',
    'context_stats',
    'delivered',
    'diff_content',
    'done',
    'error',
    'interrupted',
    'llm_call_metrics',
    'notice',
    'reasoning',
    'session_init',
    'session_state_changed',
    'sync_session',
    'text',
    'tool_call',
    'tool_call_result',
    'tool_call_stream',
    'tool_result_turn',
    'turn_result',
    'turn_started',
    'user_message_accepted',
  ];

  it('covers exactly the Python registry (both directions)', () => {
    const mirror = [...KNOWN_EVENT_TYPES].sort();
    expect(mirror).toStrictEqual(PYTHON_EVENT_TYPES);
    expect(PYTHON_EVENT_TYPES).toHaveLength(24);
  });

  it('isKnownEventType only accepts registry entries', () => {
    expect(isKnownEventType('text')).toBe(true);
    expect(isKnownEventType('_chunk')).toBe(false);
    expect(isKnownEventType('nope')).toBe(false);
  });
});
