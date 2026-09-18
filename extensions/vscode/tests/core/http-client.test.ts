import { describe, expect, it } from 'vitest';

import { GatewayHttpError } from '../../src/core/errors';
import {
  COMPACT_HTTP_TIMEOUT_MS,
  DEFAULT_HTTP_TIMEOUT_MS,
  GatewayHttpClient,
} from '../../src/core/http-client';
import type {
  HttpTransport,
  HttpTransportRequest,
  HttpTransportResponse,
} from '../../src/core/transport/http';

/**
 * `GatewayHttpClient` — one test per endpoint.
 *
 * The transport is scripted, so every test asserts the **exact request shape**
 * (method, path, query, headers, body) *and* the decoded response. That makes
 * this file the executable version of the endpoint table in
 * `docs/dev/http-api.md`.
 */

class ScriptedTransport implements HttpTransport {
  readonly requests: HttpTransportRequest[] = [];
  private readonly responses: HttpTransportResponse[] = [];

  /** Queue one response (FIFO). */
  reply(status: number, body: unknown): this {
    this.responses.push({ status, body: typeof body === 'string' ? body : JSON.stringify(body) });
    return this;
  }

  request(request: HttpTransportRequest): Promise<HttpTransportResponse> {
    this.requests.push(request);
    const response = this.responses.shift();
    if (response === undefined) {
      return Promise.reject(new Error('ScriptedTransport: no response queued'));
    }
    return Promise.resolve(response);
  }

  get count(): number {
    return this.requests.length;
  }

  get last(): HttpTransportRequest {
    const request = this.requests[this.requests.length - 1];
    if (request === undefined) {
      throw new Error('ScriptedTransport: no request recorded');
    }
    return request;
  }

  body(index = this.requests.length - 1): unknown {
    const request = this.requests[index];
    return request?.body === null || request?.body === undefined ? null : JSON.parse(request.body);
  }
}

function makeClient(
  transport: HttpTransport,
  options: { apiKey?: string | null; baseUrl?: string } = {},
): GatewayHttpClient {
  return new GatewayHttpClient({
    baseUrl: options.baseUrl ?? 'http://127.0.0.1:32523',
    apiKey: options.apiKey ?? null,
    transport,
  });
}

const OK = { ok: true };

describe('session lifecycle', () => {
  it('createSession posts the options and decodes the response', async () => {
    const transport = new ScriptedTransport().reply(200, {
      session_id: 'sess-1',
      template_name: 'default',
      workspace: '/tmp/ws',
      backend: 'file',
    });
    const response = await makeClient(transport).createSession({
      workspace: '/tmp/ws',
      template_name: 'default',
    });

    expect(transport.last.method).toBe('POST');
    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/create');
    expect(transport.last.headers['Content-Type']).toBe('application/json');
    expect(transport.body()).toStrictEqual({ template_name: 'default', workspace: '/tmp/ws' });
    expect(response).toStrictEqual({
      session_id: 'sess-1',
      template_name: 'default',
      workspace: '/tmp/ws',
      backend: 'file',
    });
  });

  it('createSession sends an empty body when no options are given', async () => {
    const transport = new ScriptedTransport().reply(200, {
      session_id: 's',
      template_name: '',
      workspace: null,
      backend: 'file',
    });
    await makeClient(transport).createSession();
    expect(transport.body()).toStrictEqual({});
  });

  it('createSession serialises an agent override, dropping nulls', async () => {
    const transport = new ScriptedTransport().reply(200, {
      session_id: 's',
      template_name: '',
      workspace: null,
      backend: 'file',
    });
    await makeClient(transport).createSession({
      agent: {
        model: 'gpt-5',
        provider: 'openai',
        system_prompt: null,
        append_system_prompt: null,
        tools: ['Bash'],
        max_turns: null,
        effort: 'high',
        yolo: true,
      },
    });

    expect(transport.body()).toStrictEqual({
      agent: { model: 'gpt-5', provider: 'openai', tools: ['Bash'], effort: 'high', yolo: true },
    });
  });

  it('resumeSession / forkSession', async () => {
    const transport = new ScriptedTransport()
      .reply(200, { session_id: 'sess-1', template_name: 'default', workspace: null })
      .reply(200, { session_id: 'sess-2', draft: 'half typed' });
    const client = makeClient(transport);

    await expect(client.resumeSession('sess-1')).resolves.toStrictEqual({
      session_id: 'sess-1',
      template_name: 'default',
      workspace: null,
    });
    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/resume');
    expect(transport.body()).toStrictEqual({ session_id: 'sess-1' });

    await expect(client.forkSession('sess-1', 'uuid-9')).resolves.toStrictEqual({
      session_id: 'sess-2',
      draft: 'half typed',
    });
    expect(transport.body()).toStrictEqual({ source_session_id: 'sess-1', target_uuid: 'uuid-9' });
  });
});

describe('subscription endpoints', () => {
  it('subscribe sends X-Client-Id and decodes the ack', async () => {
    const transport = new ScriptedTransport().reply(200, OK);
    const response = await makeClient(transport).subscribe('sess-1', 'client-7');

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/subscribe');
    expect(transport.last.headers['X-Client-Id']).toBe('client-7');
    expect(transport.body()).toStrictEqual({ session_id: 'sess-1' });
    expect(response).toStrictEqual({ ok: true });
  });

  it('unsubscribe sends X-Client-Id', async () => {
    const transport = new ScriptedTransport().reply(200, OK);
    await makeClient(transport).unsubscribe('sess-1', 'client-7');

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/unsubscribe');
    expect(transport.last.headers['X-Client-Id']).toBe('client-7');
  });
});

describe('messaging and queries', () => {
  it('sendMessage posts content and an optional tool_call_id', async () => {
    const transport = new ScriptedTransport()
      .reply(200, { ok: true, request_id: 'req-9' })
      .reply(200, { ok: true, request_id: 'req-10' });
    const client = makeClient(transport);

    await expect(client.sendMessage('sess-1', 'hi')).resolves.toStrictEqual({
      ok: true,
      request_id: 'req-9',
    });
    expect(transport.body()).toStrictEqual({ session_id: 'sess-1', content: 'hi' });

    await client.sendMessage('sess-1', 'yes', 'call-3');
    expect(transport.body()).toStrictEqual({ session_id: 'sess-1', content: 'yes', tool_call_id: 'call-3' });
  });

  it('listSessions decodes the session summaries (status fallback included)', async () => {
    const transport = new ScriptedTransport().reply(200, {
      sessions: [
        {
          id: 'sess-1',
          name: 'First',
          created_at: '2026-09-18T08:00:00',
          template_name: 'default',
          workspace: '/tmp/ws',
          last_interaction: '2026-09-18T09:00:00',
          status: 'working',
        },
        { id: 'sess-2' },
      ],
    });
    const response = await makeClient(transport).listSessions();

    expect(transport.last.method).toBe('GET');
    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/list');
    expect(response.sessions[0]).toStrictEqual({
      id: 'sess-1',
      name: 'First',
      created_at: '2026-09-18T08:00:00',
      template_name: 'default',
      workspace: '/tmp/ws',
      last_interaction: '2026-09-18T09:00:00',
      status: 'working',
    });
    // An old gateway without `status` degrades instead of throwing.
    expect(response.sessions[1]).toMatchObject({ id: 'sess-2', status: 'inactive', name: null });
  });

  it('getSession decodes the message projection', async () => {
    const transport = new ScriptedTransport().reply(200, {
      session_id: 'sess-1',
      name: null,
      template_name: null,
      workspace: null,
      status: 'idle',
      messages: [
        { role: 'user', content: 'hi', uuid: 'm1' },
        {
          role: 'assistant',
          content: 'yo',
          uuid: 'm2',
          tool_calls: [{ id: 'c1', name: 'Bash', arguments: {} }],
        },
      ],
      agent: null,
    });
    const response = await makeClient(transport).getSession('sess-1');

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/get?session_id=sess-1');
    expect(response.messages).toHaveLength(2);
    expect(response.messages[1]?.tool_calls).toStrictEqual([{ id: 'c1', name: 'Bash', arguments: {} }]);
  });

  it('sessionInfo decodes the runtime status', async () => {
    const transport = new ScriptedTransport().reply(200, {
      model: 'gpt-5',
      api_url: 'https://api.example.com',
      tools: ['Bash', 'Read'],
      total_tokens: 1200,
      context_window_tokens: 200_000,
      thinking: true,
      reasoning_effort: 'high',
      yolo: false,
      session_name: 'Work',
      workdir: '/tmp/ws',
      status: 'working',
      context_stats: { message_count: 7, total_tokens: 1200 },
      skills_info: 'loaded 2 skills',
      system_prompt: 'be brief',
    });
    const response = await makeClient(transport).sessionInfo('sess-1');

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/info?session_id=sess-1');
    expect(response).toMatchObject({
      model: 'gpt-5',
      thinking: true,
      reasoning_effort: 'high',
      context_stats: { message_count: 7, total_tokens: 1200 },
      workdir: '/tmp/ws',
    });
  });

  it('sessionBranches decodes branch targets', async () => {
    const transport = new ScriptedTransport().reply(200, { targets: [{ uuid: 'u1', content: 'first' }] });
    const response = await makeClient(transport).sessionBranches('sess-1');

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/branches?session_id=sess-1');
    expect(response.targets).toStrictEqual([{ uuid: 'u1', content: 'first', role: 'user' }]);
  });
});

describe('session mutations', () => {
  it('updateSession sends only the provided fields', async () => {
    const transport = new ScriptedTransport().reply(200, OK);
    await makeClient(transport).updateSession({ session_id: 'sess-1', thinking: false, title: 'New' });

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/update');
    expect(transport.body()).toStrictEqual({ session_id: 'sess-1', thinking: false, title: 'New' });
  });

  it('updateSession keeps model + provider + tools together', async () => {
    const transport = new ScriptedTransport().reply(200, OK);
    await makeClient(transport).updateSession({
      session_id: 'sess-1',
      model: 'claude-sonnet-4',
      provider: 'anthropic',
      tools: ['Bash', 'default.Read'],
      yolo: true,
    });

    expect(transport.body()).toStrictEqual({
      session_id: 'sess-1',
      model: 'claude-sonnet-4',
      provider: 'anthropic',
      tools: ['Bash', 'default.Read'],
      yolo: true,
    });
  });

  it('compactSession uses the long deadline and sends the instruction', async () => {
    const transport = new ScriptedTransport().reply(200, {
      ok: true,
      original_tokens: 900,
      compressed_tokens: 120,
    });
    const response = await makeClient(transport).compactSession('sess-1', 'keep the decisions');

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/compact');
    expect(transport.last.timeoutMs).toBe(COMPACT_HTTP_TIMEOUT_MS);
    expect(transport.body()).toStrictEqual({ session_id: 'sess-1', instruction: 'keep the decisions' });
    expect(response).toStrictEqual({ ok: true, original_tokens: 900, compressed_tokens: 120 });
  });

  it('interrupt / rewind', async () => {
    const transport = new ScriptedTransport().reply(200, OK).reply(200, { ok: true, draft: 'redo this' });
    const client = makeClient(transport);

    await client.interruptSession('sess-1');
    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/session/interrupt');
    expect(transport.body()).toStrictEqual({ session_id: 'sess-1' });

    await expect(client.rewindSession('sess-1', 'uuid-3')).resolves.toStrictEqual({
      ok: true,
      draft: 'redo this',
    });
    expect(transport.body()).toStrictEqual({ session_id: 'sess-1', target_uuid: 'uuid-3' });
  });

  it('every non-compact request uses the default timeout', async () => {
    const transport = new ScriptedTransport().reply(200, OK);
    await makeClient(transport).interruptSession('sess-1');
    expect(transport.last.timeoutMs).toBe(DEFAULT_HTTP_TIMEOUT_MS);
  });
});

describe('system endpoints', () => {
  it('listCommands decodes prompt commands', async () => {
    const transport = new ScriptedTransport().reply(200, {
      commands: [{ name: 'init', aliases: [], description: 'Initialize', params: '' }],
    });
    const response = await makeClient(transport).listCommands();

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/commands');
    expect(response.commands).toStrictEqual([
      { name: 'init', aliases: [], description: 'Initialize', params: '' },
    ]);
  });

  it('listModels returns provider groups', async () => {
    const transport = new ScriptedTransport().reply(200, {
      providers: [{ provider: 'openai', models: ['gpt-5'] }],
    });
    const response = await makeClient(transport).listModels();

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/models');
    expect(response.providers).toStrictEqual([{ provider: 'openai', models: ['gpt-5'] }]);
  });

  it('listAgents / listTools', async () => {
    const transport = new ScriptedTransport()
      .reply(200, { agents: ['default', 'explorer'], default_agent: 'default' })
      .reply(200, {
        tools: [
          {
            ref: 'default.Bash',
            namespace: 'default',
            name: 'Bash',
            llm_name: 'Bash',
            description: 'run a command',
          },
        ],
      });
    const client = makeClient(transport);

    await expect(client.listAgents()).resolves.toStrictEqual({
      agents: ['default', 'explorer'],
      default_agent: 'default',
    });
    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/agents');

    const tools = await client.listTools();
    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/tools');
    expect(tools.tools[0]).toMatchObject({ ref: 'default.Bash', description: 'run a command' });
  });

  it('reloadSystem decodes per-item results', async () => {
    const transport = new ScriptedTransport().reply(200, {
      ok: false,
      results: [
        { name: 'config', ok: true },
        { name: 'hooks', ok: false, detail: 'syntax error' },
      ],
    });
    const response = await makeClient(transport).reloadSystem();

    expect(transport.last.method).toBe('POST');
    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/system/reload');
    expect(response.results[1]).toStrictEqual({ name: 'hooks', ok: false, detail: 'syntax error' });
  });

  it('shutdown decodes the bare status body', async () => {
    const transport = new ScriptedTransport().reply(200, { status: 'shutting_down' });
    await expect(makeClient(transport).shutdown()).resolves.toStrictEqual({ status: 'shutting_down' });
  });

  it('health decodes version / commit / uptime', async () => {
    const transport = new ScriptedTransport().reply(200, {
      service: 'wing-gateway',
      status: 'ok',
      version: '0.4.2',
      uptime: 42,
    });
    const response = await makeClient(transport).health();

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/health');
    expect(response).toStrictEqual({
      service: 'wing-gateway',
      status: 'ok',
      version: '0.4.2',
      commit: null,
      uptime: 42,
    });
  });

  it('registerTools sends X-Client-Id and the specs', async () => {
    const transport = new ScriptedTransport().reply(200, { ok: true, registered: ['vi.Bash'] });
    await makeClient(transport).registerTools('vi', [
      { name: 'Bash', description: 'run', llm_name: 'ViBash', params: [{ name: 'command', type: 'string' }] },
    ]);

    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/tools/register');
    expect(transport.last.headers['X-Client-Id']).toBe('vi');
    expect(transport.body()).toStrictEqual({
      tools: [
        {
          name: 'Bash',
          description: 'run',
          llm_name: 'ViBash',
          params: [{ name: 'command', type: 'string' }],
        },
      ],
    });
  });
});

describe('auth and base url', () => {
  it('adds the Authorization header only when a key is configured', async () => {
    const withKey = new ScriptedTransport().reply(200, { status: 'ok', version: '1', uptime: 1 });
    await makeClient(withKey, { apiKey: 'secret' }).health();
    expect(withKey.last.headers['Authorization']).toBe('Bearer secret');

    const withoutKey = new ScriptedTransport().reply(200, { status: 'ok', version: '1', uptime: 1 });
    await makeClient(withoutKey, { apiKey: '   ' }).health();
    expect(withoutKey.last.headers['Authorization']).toBeUndefined();
  });

  it('normalises a trailing slash on the base url', async () => {
    const transport = new ScriptedTransport().reply(200, { status: 'ok', version: '1', uptime: 1 });
    await makeClient(transport, { baseUrl: 'http://127.0.0.1:32523/' }).health();
    expect(transport.last.url).toBe('http://127.0.0.1:32523/api/health');
  });
});

describe('error surface', () => {
  it('maps a structured error body into GatewayHttpError', async () => {
    const transport = new ScriptedTransport().reply(404, {
      error: 'not_found',
      detail: 'session not found',
      session_id: 'sess-1',
    });
    const client = makeClient(transport);

    const error = await client.getSession('sess-1').catch((cause: unknown) => cause);
    expect(error).toBeInstanceOf(Error);
    expect(error).toMatchObject({
      name: 'GatewayHttpError',
      kind: 'http',
      status: 404,
      body: { error: 'not_found', detail: 'session not found', session_id: 'sess-1', uuid: null },
      rawBody: expect.stringContaining('session not found'),
    });
    expect((error as { isNotFound(): boolean }).isNotFound()).toBe(true);
    expect((error as { isUnauthorized(): boolean }).isUnauthorized()).toBe(false);
    expect((error as { message: string }).message).toContain('session not found');
  });

  it('keeps the raw body when the error shape is unknown', async () => {
    const transport = new ScriptedTransport().reply(502, '<html>bad gateway</html>');
    const error = await makeClient(transport)
      .health()
      .catch((cause: unknown) => cause);

    expect(error).toMatchObject({
      kind: 'http',
      status: 502,
      body: null,
      rawBody: '<html>bad gateway</html>',
    });
    expect((error as { detail: string }).detail).toBe('<html>bad gateway</html>');
  });

  it('flags 401/403 as unauthorized', async () => {
    const transport = new ScriptedTransport().reply(401, { error: 'unauthorized' });
    const error = await makeClient(transport)
      .listSessions()
      .catch((cause: unknown) => cause);

    expect(error).toMatchObject({ status: 401, body: { error: 'unauthorized' } });
    expect((error as { isUnauthorized(): boolean }).isUnauthorized()).toBe(true);
  });

  it('reports an undecodable 2xx body as malformed-response', async () => {
    const transport = new ScriptedTransport().reply(200, { nope: true });
    const error = await makeClient(transport)
      .health()
      .catch((cause: unknown) => cause);

    expect(error).toMatchObject({ kind: 'malformed-response', status: 200 });
    expect((error as { rawBody: string }).rawBody).toContain('nope');
  });

  it('reports the real status of a malformed success response (review r1 N4)', async () => {
    const transport = new ScriptedTransport().reply(202, { nope: true });
    const error = await makeClient(transport)
      .health()
      .catch((cause: unknown) => cause);
    expect(error).toMatchObject({ kind: 'malformed-response', status: 202 });

    const empty = await makeClient(new ScriptedTransport().reply(204, ''))
      .health()
      .catch((cause: unknown) => cause);
    expect(empty).toMatchObject({ kind: 'malformed-response', status: 204 });
  });

  it('does not invent a required collection (review r1 S1 class check)', async () => {
    // Python marks these as required (no `default_factory`), so an absent key is a
    // malformed response — guessing `[]` would hide a protocol break.
    const missingSessions = await makeClient(new ScriptedTransport().reply(200, {}))
      .listSessions()
      .catch((cause: unknown) => cause);
    expect(missingSessions).toMatchObject({ kind: 'malformed-response' });

    const missingMessages = await makeClient(new ScriptedTransport().reply(200, { session_id: 's' }))
      .getSession('s')
      .catch((cause: unknown) => cause);
    expect(missingMessages).toMatchObject({ kind: 'malformed-response' });

    const missingTools = await makeClient(
      new ScriptedTransport().reply(200, {
        model: 'gpt-5',
        api_url: '',
        total_tokens: 0,
        context_window_tokens: 0,
        thinking: false,
        yolo: false,
        context_stats: { message_count: 0, total_tokens: 0 },
      }),
    )
      .sessionInfo('s')
      .catch((cause: unknown) => cause);
    expect(missingTools).toMatchObject({ kind: 'malformed-response' });
  });

  it('reports a non-JSON 2xx body as malformed-response', async () => {
    const transport = new ScriptedTransport().reply(200, 'not json');
    const error = await makeClient(transport)
      .health()
      .catch((cause: unknown) => cause);
    expect(error).toMatchObject({ kind: 'malformed-response' });
  });

  it('wraps a transport failure in a typed error, keeping the cause (review r1 N5)', async () => {
    const transport = new ScriptedTransport();
    const error = await makeClient(transport)
      .health()
      .catch((cause: unknown) => cause);

    expect(error).toMatchObject({
      name: 'GatewayHttpError',
      kind: 'network',
      status: null,
      message: expect.stringContaining('no response queued'),
    });
    expect((error as { cause: unknown }).cause).toBeInstanceOf(Error);
    expect(transport.count).toBe(1);
  });

  it('lets a GatewayHttpError from the transport pass through untouched (review r1 N5)', async () => {
    const timeout = new GatewayHttpError({ kind: 'timeout', message: 'GET /api/health timed out' });
    const transport: HttpTransport = { request: () => Promise.reject(timeout) };

    const error = await makeClient(transport)
      .health()
      .catch((cause: unknown) => cause);
    expect(error).toBe(timeout);
  });
});
