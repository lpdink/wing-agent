import { describe, expect, it } from 'vitest';

import {
  decodeSessionMessage,
  decodeSessionMessages,
  decodeUncommittedTool,
  decodeUncommittedTools,
  hasToolCalls,
} from '../../src/core/protocol/history';

/**
 * Message projection (`wing/session.py::serialize_message`) — what replay and
 * `GET /api/session/get` hand to the host.
 */

describe('decodeSessionMessage', () => {
  it('decodes a full projection', () => {
    expect(
      decodeSessionMessage({
        role: 'assistant',
        content: 'the answer',
        uuid: 'u1',
        reasoning_content: 'thinking',
        tool_calls: [{ id: 'tc1', name: 'Bash', arguments: { command: 'ls' } }],
        tool_call_id: null,
      }),
    ).toStrictEqual({
      role: 'assistant',
      content: 'the answer',
      uuid: 'u1',
      reasoning_content: 'thinking',
      tool_calls: [{ id: 'tc1', name: 'Bash', arguments: { command: 'ls' } }],
      tool_call_id: null,
    });
  });

  it('defaults absent optionals (old records)', () => {
    expect(decodeSessionMessage({ role: 'user', content: 'hi', uuid: null })).toStrictEqual({
      role: 'user',
      content: 'hi',
      uuid: null,
      reasoning_content: null,
      tool_calls: [],
      tool_call_id: null,
    });
  });

  it('ignores unknown fields (forward tolerant)', () => {
    expect(decodeSessionMessage({ role: 'user', content: 'hi', from_the_future: 1 })).toMatchObject({
      role: 'user',
      content: 'hi',
    });
  });

  it('keeps an explicit null tool_call_id apart from an absent one', () => {
    expect(decodeSessionMessage({ role: 'tool', content: 'x', tool_call_id: null })?.tool_call_id).toBeNull();
    expect(decodeSessionMessage({ role: 'tool', content: 'x' })?.tool_call_id).toBeNull();
    expect(decodeSessionMessage({ role: 'tool', content: 'x', tool_call_id: 'c1' })?.tool_call_id).toBe('c1');
  });

  it('skips tool calls without an id or a name', () => {
    const message = decodeSessionMessage({
      role: 'assistant',
      content: '',
      tool_calls: [{ id: 'ok', name: 'Bash' }, { id: 'no-name' }, { name: 'no-id' }, 'garbage'],
    });
    expect(message?.tool_calls).toStrictEqual([{ id: 'ok', name: 'Bash', arguments: null }]);
  });

  it('rejects payloads that are not objects', () => {
    expect(decodeSessionMessage('nope')).toBeNull();
    expect(decodeSessionMessage(['role'])).toBeNull();
    expect(decodeSessionMessage(null)).toBeNull();
  });

  it('reports whether the message carries tool calls', () => {
    expect(
      hasToolCalls(decodeSessionMessage({ role: 'assistant', tool_calls: [{ id: 'a', name: 'B' }] })!),
    ).toBe(true);
    expect(hasToolCalls(decodeSessionMessage({ role: 'assistant' })!)).toBe(false);
  });
});

describe('decodeSessionMessages', () => {
  it('skips entries that do not decode', () => {
    const messages = decodeSessionMessages([
      { role: 'user', content: 'a' },
      42,
      { role: 'assistant', content: 'b' },
    ]);
    expect(messages.map((message) => message.content)).toStrictEqual(['a', 'b']);
  });
});

describe('uncommitted tool calls', () => {
  it('decodes the streaming args fragment', () => {
    expect(
      decodeUncommittedTools([{ tool_call_id: 'c1', tool_name: 'Write', args_fragment: '{"path"' }]),
    ).toStrictEqual([{ tool_call_id: 'c1', tool_name: 'Write', args_fragment: '{"path"' }]);
  });

  it('defaults a missing fragment and skips entries without ids', () => {
    expect(decodeUncommittedTool({ tool_call_id: 'c1', tool_name: 'Write' })).toStrictEqual({
      tool_call_id: 'c1',
      tool_name: 'Write',
      args_fragment: '',
    });
    expect(decodeUncommittedTool({ tool_name: 'Write' })).toBeNull();
    expect(decodeUncommittedTool('nope')).toBeNull();
  });
});
