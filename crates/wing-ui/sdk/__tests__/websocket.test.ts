import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { TextEvent, ToolCallEvent } from '../src/events'
import { WebSocketClient } from '../src/index'

// ============================================================
// Mock WebSocket
// ============================================================

type WsHandler = ((ev: unknown) => void) | null

class MockWebSocket {
  static CONNECTING = 0
  static OPEN = 1
  static CLOSING = 2
  static CLOSED = 3

  readyState = MockWebSocket.CONNECTING
  onopen: WsHandler = null
  onclose: WsHandler = null
  onmessage: WsHandler = null
  onerror: WsHandler = null

  sent: string[] = []

  constructor(_url: string) {
    // Auto-connect after microtask
    queueMicrotask(() => {
      this.readyState = MockWebSocket.OPEN
      this.onopen?.({})
    })
  }

  send(data: string): void {
    this.sent.push(data)
  }

  close(): void {
    this.readyState = MockWebSocket.CLOSED
    this.onclose?.({})
  }

  // Test helpers
  simulateMessage(data: unknown): void {
    this.onmessage?.({ data: JSON.stringify(data) })
  }

  simulateClose(): void {
    this.readyState = MockWebSocket.CLOSED
    this.onclose?.({})
  }

  simulateError(): void {
    this.onerror?.({})
  }
}

let instances: MockWebSocket[] = []

function installMockWebSocket(): void {
  instances = []
  vi.stubGlobal(
    'WebSocket',
    class extends MockWebSocket {
      constructor(url: string) {
        super(url)
        instances.push(this)
      }
    },
  )
}

// ============================================================
// Tests
// ============================================================

describe('WebSocketClient', () => {
  beforeEach(() => {
    installMockWebSocket()
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.restoreAllMocks()
    vi.useRealTimers()
  })

  function getWs(): MockWebSocket {
    return instances[instances.length - 1]!
  }

  // ── Connection ───────────────────────────────────────────

  describe('connect', () => {
    it('opens a WebSocket and receives ConnectResponse', async () => {
      const client = new WebSocketClient({ url: 'ws://localhost:32523/ws' })

      client.connect()
      await vi.runAllTimersAsync()

      // Simulate ConnectResponse
      getWs().simulateMessage({ type: 'connected', client_id: 'abc123' })

      expect(client.clientId).toBe('abc123')
      expect(client.status).toBe('connected')
    })
  })

  describe('disconnect', () => {
    it('closes without reconnecting', async () => {
      const client = new WebSocketClient({ url: 'ws://localhost:32523/ws' })

      client.connect()
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'abc' })

      client.disconnect()

      expect(client.status).toBe('disconnected')
      expect(client.clientId).toBeNull()

      // No reconnect should be scheduled
      await vi.runAllTimersAsync()
      expect(instances).toHaveLength(1)
    })
  })

  // ── Event dispatch ───────────────────────────────────────

  describe('event dispatch', () => {
    it('dispatches typed events to registered handlers', async () => {
      const client = new WebSocketClient({ url: 'ws://localhost:32523/ws' })
      const textHandler = vi.fn()
      const toolHandler = vi.fn()

      client.on('text', textHandler)
      client.on('tool_call', toolHandler)

      client.connect()
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })

      // Send a text event
      const textEvent = {
        type: 'text',
        content: 'Hello!',
        created_at: '2026-01-01T00:00:00Z',
        session_id: 's1',
        request_id: 'r1',
      }
      getWs().simulateMessage(textEvent)

      expect(textHandler).toHaveBeenCalledWith(
        expect.objectContaining({ type: 'text', content: 'Hello!' }),
      )
      expect(toolHandler).not.toHaveBeenCalled()

      // Send a tool_call event
      const toolEvent = {
        type: 'tool_call',
        tool_name: 'Bash',
        tool_args: { command: 'ls' },
        tool_call_id: 'tc1',
        created_at: '2026-01-01T00:00:00Z',
        session_id: 's1',
        request_id: 'r2',
      }
      getWs().simulateMessage(toolEvent)

      expect(toolHandler).toHaveBeenCalledWith(
        expect.objectContaining({ type: 'tool_call', tool_name: 'Bash' }),
      )
      expect(textHandler).toHaveBeenCalledTimes(1) // still 1
    })

    it('supports off() to remove handlers', async () => {
      const client = new WebSocketClient({ url: 'ws://localhost:32523/ws' })
      const handler = vi.fn()

      client.on('text', handler)
      client.connect()
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })

      getWs().simulateMessage({
        type: 'text',
        content: 'first',
        created_at: '',
        session_id: null,
        request_id: '',
      })
      expect(handler).toHaveBeenCalledTimes(1)

      client.off('text', handler)

      getWs().simulateMessage({
        type: 'text',
        content: 'second',
        created_at: '',
        session_id: null,
        request_id: '',
      })
      expect(handler).toHaveBeenCalledTimes(1) // still 1
    })

    it('tolerates handler errors without breaking connection', async () => {
      const client = new WebSocketClient({ url: 'ws://localhost:32523/ws' })
      const badHandler = vi.fn().mockImplementation(() => {
        throw new Error('handler error')
      })
      const goodHandler = vi.fn()

      client.on('text', badHandler)
      client.on('text', goodHandler)

      client.connect()
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })

      getWs().simulateMessage({
        type: 'text',
        content: 'test',
        created_at: '',
        session_id: null,
        request_id: '',
      })

      expect(badHandler).toHaveBeenCalled()
      expect(goodHandler).toHaveBeenCalled()
    })
  })

  // ── Send ─────────────────────────────────────────────────

  describe('send', () => {
    it('sends a ClientRequest over WebSocket', async () => {
      const client = new WebSocketClient({ url: 'ws://localhost:32523/ws' })

      client.connect()
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })

      client.send('session-1', 'Hello Gateway', 'req-123')

      const sent = JSON.parse(getWs().sent[0]!)
      expect(sent.session_id).toBe('session-1')
      expect(sent.content).toBe('Hello Gateway')
      expect(sent.request_id).toBe('req-123')
    })

    it('throws when not connected', () => {
      const client = new WebSocketClient({ url: 'ws://localhost:32523/ws' })

      expect(() => client.send('s1', 'hello')).toThrow('not connected')
    })
  })

  // ── Reconnection ─────────────────────────────────────────

  describe('reconnection', () => {
    it('reconnects with exponential backoff on unexpected close', async () => {
      const client = new WebSocketClient({
        url: 'ws://localhost:32523/ws',
        reconnectDelayMs: 100,
        maxReconnectDelayMs: 800,
      })
      const statusChanges: string[] = []
      client.onStatusChange((s) => statusChanges.push(s))

      client.connect()
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })

      expect(client.status).toBe('connected')

      // Simulate unexpected close
      getWs().simulateClose()

      expect(client.status).toBe('reconnecting')

      // First reconnect attempt after 100ms
      await vi.advanceTimersByTimeAsync(100)
      expect(instances).toHaveLength(2)

      // Close again WITHOUT connecting — backoff should be 200ms now
      getWs().simulateClose()
      await vi.advanceTimersByTimeAsync(100)
      expect(instances).toHaveLength(2) // not yet (need 200ms total)
      await vi.advanceTimersByTimeAsync(100)
      expect(instances).toHaveLength(3) // now at 200ms
    })

    it('does not reconnect after intentional disconnect', async () => {
      const client = new WebSocketClient({
        url: 'ws://localhost:32523/ws',
        reconnectDelayMs: 100,
      })

      client.connect()
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })

      client.disconnect()
      await vi.advanceTimersByTimeAsync(500)

      expect(instances).toHaveLength(1) // No reconnect
    })

    it('resets backoff after successful reconnect', async () => {
      const client = new WebSocketClient({
        url: 'ws://localhost:32523/ws',
        reconnectDelayMs: 100,
        maxReconnectDelayMs: 800,
      })

      client.connect()
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })

      // Force 3 reconnections to build up delay
      getWs().simulateClose()
      await vi.advanceTimersByTimeAsync(100) // 100ms
      getWs().simulateClose()
      await vi.advanceTimersByTimeAsync(200) // 200ms
      getWs().simulateClose()
      await vi.advanceTimersByTimeAsync(400) // 400ms

      // Now connect successfully
      getWs().simulateMessage({ type: 'connected', client_id: 'final' })
      expect(client.clientId).toBe('final')

      // Close again — delay should reset to 100ms
      getWs().simulateClose()
      await vi.advanceTimersByTimeAsync(100)
      expect(instances.length).toBeGreaterThan(4)
    })
  })

  // ── Status change ────────────────────────────────────────

  describe('status changes', () => {
    it('notifies status handlers on transitions', async () => {
      const client = new WebSocketClient({
        url: 'ws://localhost:32523/ws',
        reconnectDelayMs: 100,
      })
      const statuses: string[] = []
      client.onStatusChange((s) => statuses.push(s))

      client.connect()
      expect(statuses).toContain('connecting')

      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })
      expect(statuses).toContain('connected')

      client.disconnect()
      expect(statuses).toContain('disconnected')
    })

    it('supports offStatusChange', async () => {
      const client = new WebSocketClient({ url: 'ws://localhost:32523/ws' })
      const handler = vi.fn()

      client.onStatusChange(handler)
      client.connect()
      expect(handler).toHaveBeenCalledTimes(1) // connecting

      client.offStatusChange(handler)
      await vi.runAllTimersAsync()
      getWs().simulateMessage({ type: 'connected', client_id: 'c1' })
      expect(handler).toHaveBeenCalledTimes(1) // still 1
    })
  })

  // ── Type narrowing validation ────────────────────────────

  describe('type narrowing', () => {
    it('discriminated union narrows correctly', () => {
      // This is a compile-time check that also runs at runtime
      const events: Array<TextEvent | ToolCallEvent> = []

      const textEvent: TextEvent = {
        type: 'text',
        content: 'hello',
        created_at: '',
        session_id: null,
        request_id: '',
      }
      events.push(textEvent)

      for (const event of events) {
        if (event.type === 'text') {
          // TS should allow .content access
          expect(typeof event.content).toBe('string')
        }
        if (event.type === 'tool_call') {
          // TS should allow .tool_name access
          expect(typeof event.tool_name).toBe('string')
        }
      }
    })
  })
})
