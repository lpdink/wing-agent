import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ApiClientError, GatewayClient } from '../src/index'

const BASE_URL = 'http://127.0.0.1:32523'

function mockFetch(data: unknown, status = 200): ReturnType<typeof vi.fn> {
  return vi.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    json: async () => data,
    text: async () => JSON.stringify(data),
  })
}

function mockFetchError(status: number, error: string, detail: string): ReturnType<typeof vi.fn> {
  const body = { error, detail, session_id: null, uuid: null }
  return vi.fn().mockResolvedValue({
    ok: false,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  })
}

describe('GatewayClient', () => {
  let client: GatewayClient

  beforeEach(() => {
    client = new GatewayClient({ baseUrl: BASE_URL, clientId: 'test-client' })
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  // ── Session Lifecycle ────────────────────────────────────

  describe('createSession', () => {
    it('sends POST to /api/session/create with body', async () => {
      const expected = {
        session_id: 's1',
        template_name: 'default',
        workspace: null,
      }
      const fetch = mockFetch(expected)
      vi.stubGlobal('fetch', fetch)

      const result = await client.createSession({
        template_name: 'default',
        workspace: '/tmp',
      })

      expect(result).toEqual(expected)
      expect(fetch).toHaveBeenCalledWith(
        `${BASE_URL}/api/session/create`,
        expect.objectContaining({
          method: 'POST',
          body: JSON.stringify({ template_name: 'default', workspace: '/tmp' }),
        }),
      )
    })

    it('works with empty request', async () => {
      const expected = {
        session_id: 's2',
        template_name: 'default',
        workspace: null,
      }
      vi.stubGlobal('fetch', mockFetch(expected))

      const result = await client.createSession()
      expect(result.session_id).toBe('s2')
    })
  })

  describe('resumeSession', () => {
    it('sends POST with session_id', async () => {
      const expected = {
        session_id: 's1',
        template_name: 'default',
        workspace: null,
      }
      const fetch = mockFetch(expected)
      vi.stubGlobal('fetch', fetch)

      const result = await client.resumeSession('s1')

      expect(result).toEqual(expected)
      const body = JSON.parse(fetch.mock.calls[0][1].body)
      expect(body.session_id).toBe('s1')
    })
  })

  describe('forkSession', () => {
    it('sends POST with source_session_id and target_uuid', async () => {
      const expected = { session_id: 's2', draft: 'hello' }
      const fetch = mockFetch(expected)
      vi.stubGlobal('fetch', fetch)

      const result = await client.forkSession('s1', 'uuid-123')

      expect(result).toEqual(expected)
      const body = JSON.parse(fetch.mock.calls[0][1].body)
      expect(body.source_session_id).toBe('s1')
      expect(body.target_uuid).toBe('uuid-123')
    })
  })

  // ── Subscription Management ──────────────────────────────

  describe('subscribe', () => {
    it('sends POST with X-Client-Id header', async () => {
      const fetch = mockFetch({ ok: true })
      vi.stubGlobal('fetch', fetch)

      await client.subscribe('s1')

      const headers = fetch.mock.calls[0][1].headers
      expect(headers['X-Client-Id']).toBe('test-client')
      const body = JSON.parse(fetch.mock.calls[0][1].body)
      expect(body.session_id).toBe('s1')
    })

    it('throws when clientId is null', async () => {
      client = new GatewayClient({ baseUrl: BASE_URL })
      vi.stubGlobal('fetch', mockFetch({ ok: true }))

      await expect(client.subscribe('s1')).rejects.toThrow(ApiClientError)
    })
  })

  describe('unsubscribe', () => {
    it('sends POST with X-Client-Id header', async () => {
      const fetch = mockFetch({ ok: true })
      vi.stubGlobal('fetch', fetch)

      await client.unsubscribe('s1')

      const headers = fetch.mock.calls[0][1].headers
      expect(headers['X-Client-Id']).toBe('test-client')
    })
  })

  // ── Message Sending ──────────────────────────────────────

  describe('sendMessage', () => {
    it('sends POST with session_id and content', async () => {
      const expected = { ok: true, request_id: 'req-1' }
      const fetch = mockFetch(expected)
      vi.stubGlobal('fetch', fetch)

      const result = await client.sendMessage('s1', 'hello world')

      expect(result).toEqual(expected)
      const body = JSON.parse(fetch.mock.calls[0][1].body)
      expect(body.session_id).toBe('s1')
      expect(body.content).toBe('hello world')
    })
  })

  // ── Queries ──────────────────────────────────────────────

  describe('listSessions', () => {
    it('sends GET to /api/session/list', async () => {
      const expected = { sessions: [] }
      const fetch = mockFetch(expected)
      vi.stubGlobal('fetch', fetch)

      const result = await client.listSessions()

      expect(result).toEqual(expected)
      expect(fetch.mock.calls[0][1]).toBeUndefined() // No options = GET
    })
  })

  describe('getSession', () => {
    it('sends GET with session_id query param', async () => {
      const expected = {
        session_id: 's1',
        name: null,
        template_name: null,
        workspace: null,
        messages: [],
        agent: null,
      }
      const fetch = mockFetch(expected)
      vi.stubGlobal('fetch', fetch)

      const result = await client.getSession('s1')

      expect(result).toEqual(expected)
      const url = new URL(fetch.mock.calls[0][0])
      expect(url.searchParams.get('session_id')).toBe('s1')
    })
  })

  describe('getSessionInfo', () => {
    it('sends GET with session_id query param', async () => {
      const fetch = mockFetch({
        model: 'gpt-4',
        api_url: 'https://api.openai.com',
        tools: [],
        total_tokens: 0,
        context_window_tokens: 128000,
        thinking: false,
        reasoning_effort: null,
        yolo: false,
        session_name: null,
        context_stats: { message_count: 0, total_tokens: 0 },
        skills_info: '',
        system_prompt: '',
      })
      vi.stubGlobal('fetch', fetch)

      const result = await client.getSessionInfo('s1')

      expect(result.model).toBe('gpt-4')
    })
  })

  describe('getBranches', () => {
    it('sends GET with session_id query param', async () => {
      const expected = { targets: [] }
      const fetch = mockFetch(expected)
      vi.stubGlobal('fetch', fetch)

      const result = await client.getBranches('s1')

      expect(result).toEqual(expected)
    })
  })

  // ── Session Update ───────────────────────────────────────

  describe('updateSession', () => {
    it('sends POST with update fields', async () => {
      const fetch = mockFetch({ ok: true })
      vi.stubGlobal('fetch', fetch)

      await client.updateSession({
        session_id: 's1',
        model: 'gpt-4o',
        thinking: true,
      })

      const body = JSON.parse(fetch.mock.calls[0][1].body)
      expect(body.session_id).toBe('s1')
      expect(body.model).toBe('gpt-4o')
      expect(body.thinking).toBe(true)
    })
  })

  // ── Session Operations ───────────────────────────────────

  describe('compactSession', () => {
    it('sends POST with session_id', async () => {
      const fetch = mockFetch({
        ok: true,
        original_tokens: 5000,
        compressed_tokens: 2000,
      })
      vi.stubGlobal('fetch', fetch)

      const result = await client.compactSession('s1')

      expect(result.original_tokens).toBe(5000)
    })
  })

  describe('interruptSession', () => {
    it('sends POST with session_id', async () => {
      const fetch = mockFetch({ ok: true })
      vi.stubGlobal('fetch', fetch)

      const result = await client.interruptSession('s1')

      expect(result.ok).toBe(true)
    })
  })

  describe('rewindSession', () => {
    it('sends POST with session_id and target_uuid', async () => {
      const fetch = mockFetch({ ok: true, draft: 'draft msg' })
      vi.stubGlobal('fetch', fetch)

      const result = await client.rewindSession('s1', 'uuid-1')

      expect(result.draft).toBe('draft msg')
      const body = JSON.parse(fetch.mock.calls[0][1].body)
      expect(body.target_uuid).toBe('uuid-1')
    })
  })

  // ── System ───────────────────────────────────────────────

  describe('health', () => {
    it('sends GET to /api/health', async () => {
      const expected = {
        service: 'wing-gateway',
        status: 'ok',
        version: '0.4.0',
        uptime: 120,
      }
      vi.stubGlobal('fetch', mockFetch(expected))

      const result = await client.health()

      expect(result).toEqual(expected)
    })
  })

  describe('getCommands', () => {
    it('sends GET to /api/commands', async () => {
      const expected = { commands: [] }
      vi.stubGlobal('fetch', mockFetch(expected))

      const result = await client.getCommands()

      expect(result).toEqual(expected)
    })
  })

  describe('getModels', () => {
    it('sends GET to /api/models', async () => {
      const expected = { models: ['gpt-4', 'gpt-4o'] }
      vi.stubGlobal('fetch', mockFetch(expected))

      const result = await client.getModels()

      expect(result.models).toHaveLength(2)
    })
  })

  describe('getAgents', () => {
    it('sends GET to /api/agents', async () => {
      const expected = { agents: ['default', 'coder'], default_agent: 'default' }
      vi.stubGlobal('fetch', mockFetch(expected))

      const result = await client.getAgents()

      expect(result.default_agent).toBe('default')
    })
  })

  describe('reloadSystem', () => {
    it('sends POST to /api/system/reload (no body)', async () => {
      const expected = { ok: true, results: [] }
      const fetch = mockFetch(expected)
      vi.stubGlobal('fetch', fetch)

      const result = await client.reloadSystem()

      expect(result.ok).toBe(true)
      expect(fetch.mock.calls[0][1].method).toBe('POST')
    })
  })

  describe('shutdown', () => {
    it('sends POST to /api/shutdown', async () => {
      const fetch = mockFetch({ status: 'shutting_down' })
      vi.stubGlobal('fetch', fetch)

      await client.shutdown()

      expect(fetch.mock.calls[0][1].method).toBe('POST')
    })
  })

  // ── Error Handling ───────────────────────────────────────

  describe('error handling', () => {
    it("throws ApiClientError with kind 'api' on non-2xx", async () => {
      vi.stubGlobal('fetch', mockFetchError(404, 'not_found', 'session not found'))

      try {
        await client.getSession('nonexistent')
        expect.fail('should have thrown')
      } catch (e) {
        expect(e).toBeInstanceOf(ApiClientError)
        const err = e as ApiClientError
        expect(err.kind).toBe('api')
        expect(err.status).toBe(404)
        expect(err.body?.error).toBe('not_found')
      }
    })

    it("throws ApiClientError with kind 'transport' on network failure", async () => {
      vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('Failed to fetch')))

      try {
        await client.health()
        expect.fail('should have thrown')
      } catch (e) {
        expect(e).toBeInstanceOf(ApiClientError)
        expect((e as ApiClientError).kind).toBe('transport')
      }
    })

    it("throws ApiClientError with kind 'deserialize' on invalid JSON", async () => {
      vi.stubGlobal('fetch', () =>
        Promise.resolve({
          ok: true,
          status: 200,
          json: async () => {
            throw new SyntaxError('Unexpected token')
          },
          text: async () => 'not json',
        }),
      )

      try {
        await client.health()
        expect.fail('should have thrown')
      } catch (e) {
        expect(e).toBeInstanceOf(ApiClientError)
        expect((e as ApiClientError).kind).toBe('deserialize')
      }
    })
  })

  // ── Constructor ──────────────────────────────────────────

  describe('constructor', () => {
    it('strips trailing slashes from baseUrl', async () => {
      client = new GatewayClient({ baseUrl: 'http://localhost:32523///' })
      const fetch = mockFetch({ sessions: [] })
      vi.stubGlobal('fetch', fetch)

      await client.listSessions()

      expect(fetch.mock.calls[0][0]).toBe('http://localhost:32523/api/session/list')
    })
  })
})
