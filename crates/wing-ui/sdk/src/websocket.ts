// sdk/src/websocket.ts — WebSocketClient: typed WS connection + event dispatch.
//
// Mirrors: wing/gateway/routes/ws.py

import type { WingEvent } from './events'
import type { ClientRequest } from './protocol'

// ============================================================
// Typed event map for discriminated listener API
// ============================================================

type EventMap = { [K in WingEvent['type']]: Extract<WingEvent, { type: K }> }

type EventHandler<E> = (event: E) => void

// ============================================================
// Connection status
// ============================================================

export type ConnectionStatus = 'disconnected' | 'connecting' | 'connected' | 'reconnecting'

// ============================================================
// WebSocketClient options
// ============================================================

export interface WebSocketClientOptions {
  /** WebSocket URL, e.g. "ws://127.0.0.1:32523/ws" */
  url: string
  /** Initial backoff delay in ms. Default: 1000. */
  reconnectDelayMs?: number
  /** Max backoff delay in ms. Default: 30000. */
  maxReconnectDelayMs?: number
}

// ============================================================
// WebSocketClient
// ============================================================

/**
 * Manages a WebSocket connection to Wing Gateway's /ws endpoint.
 *
 * - Receives `ConnectResponse` on connect (extracts `client_id`).
 * - Parses incoming JSON messages into typed `WingEvent` discriminated union.
 * - Auto-reconnects with exponential backoff on unexpected disconnects.
 * - Provides typed `on`/`off` event listener API.
 */
export class WebSocketClient {
  private url: string
  private ws: WebSocket | null = null
  private reconnectDelayMs: number
  private maxReconnectDelayMs: number
  private currentDelay: number
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null
  private intentionalClose = false

  /** Client ID assigned by Gateway. null until ConnectResponse received. */
  clientId: string | null = null

  /** Current connection status. */
  status: ConnectionStatus = 'disconnected'

  // Event listeners keyed by event type
  private handlers = new Map<string, Set<EventHandler<never>>>()

  // Status change listeners
  private statusHandlers = new Set<EventHandler<ConnectionStatus>>()

  constructor(options: WebSocketClientOptions) {
    this.url = options.url
    this.reconnectDelayMs = options.reconnectDelayMs ?? 1000
    this.maxReconnectDelayMs = options.maxReconnectDelayMs ?? 30_000
    this.currentDelay = this.reconnectDelayMs
  }

  // ============================================================
  // Lifecycle
  // ============================================================

  /** Open the WebSocket connection. */
  connect(): void {
    if (this.ws) {
      this.ws.close()
    }
    this.intentionalClose = false
    this.setStatus('connecting')

    const ws = new WebSocket(this.url)
    this.ws = ws

    ws.onopen = () => {
      // Wait for ConnectResponse before marking connected
    }

    ws.onmessage = (ev: MessageEvent) => {
      this.handleMessage(ev.data as string)
    }

    ws.onclose = () => {
      this.ws = null
      this.clientId = null
      if (!this.intentionalClose) {
        this.scheduleReconnect()
      } else {
        this.setStatus('disconnected')
      }
    }

    ws.onerror = () => {
      // onclose will fire after onerror, so reconnect is handled there
    }
  }

  /** Intentionally close the connection (no auto-reconnect). */
  disconnect(): void {
    this.intentionalClose = true
    this.clearReconnectTimer()
    if (this.ws) {
      this.ws.close()
      this.ws = null
    }
    this.clientId = null
    this.setStatus('disconnected')
  }

  /**
   * Send a message to the Gateway via WS.
   * Gateway will inject client_id and forward to WingRuntime.
   */
  send(sessionId: string, content: string, requestId?: string): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      throw new Error('WebSocket is not connected')
    }
    const req: ClientRequest = {
      request_id: requestId ?? crypto.randomUUID().replace(/-/g, ''),
      session_id: sessionId,
      content,
    }
    this.ws.send(JSON.stringify(req))
  }

  // ============================================================
  // Event listener API
  // ============================================================

  /** Register a handler for a specific event type. */
  on<K extends keyof EventMap>(type: K, handler: EventHandler<EventMap[K]>): void {
    let set = this.handlers.get(type)
    if (!set) {
      set = new Set()
      this.handlers.set(type, set)
    }
    set.add(handler as EventHandler<never>)
  }

  /** Remove a handler for a specific event type. */
  off<K extends keyof EventMap>(type: K, handler: EventHandler<EventMap[K]>): void {
    const set = this.handlers.get(type)
    if (set) {
      set.delete(handler as EventHandler<never>)
    }
  }

  /** Register a handler for connection status changes. */
  onStatusChange(handler: EventHandler<ConnectionStatus>): void {
    this.statusHandlers.add(handler)
  }

  /** Remove a status change handler. */
  offStatusChange(handler: EventHandler<ConnectionStatus>): void {
    this.statusHandlers.delete(handler)
  }

  // ============================================================
  // Internal
  // ============================================================

  private handleMessage(raw: string): void {
    let data: Record<string, unknown>
    try {
      data = JSON.parse(raw)
    } catch {
      return // Ignore non-JSON messages
    }

    // Check if it's a ConnectResponse
    if (data.type === 'connected' && data.client_id) {
      this.clientId = data.client_id as string
      this.currentDelay = this.reconnectDelayMs // Reset backoff
      this.setStatus('connected')
      return
    }

    // It's a WingEvent — dispatch by type
    const type = data.type as string | undefined
    if (!type) return

    const event = data as unknown as WingEvent
    const set = this.handlers.get(type)
    if (set) {
      for (const handler of set) {
        try {
          ;(handler as EventHandler<WingEvent>)(event)
        } catch {
          // Don't let a bad handler break the connection
        }
      }
    }
  }

  private scheduleReconnect(): void {
    this.setStatus('reconnecting')
    this.clearReconnectTimer()

    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, this.currentDelay)

    // Exponential backoff with cap
    this.currentDelay = Math.min(this.currentDelay * 2, this.maxReconnectDelayMs)
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }
  }

  private setStatus(status: ConnectionStatus): void {
    if (this.status === status) return
    this.status = status
    for (const handler of this.statusHandlers) {
      try {
        handler(status)
      } catch {
        // Don't let a bad handler break status tracking
      }
    }
  }
}
