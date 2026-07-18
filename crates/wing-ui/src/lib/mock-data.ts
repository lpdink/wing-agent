import type { SessionInfo } from '@wing-agent/sdk'

/**
 * Mock session data for Phase 3 layout development.
 * Will be replaced by real API data in Phase 4.
 */
export const mockSessions: SessionInfo[] = [
  {
    id: 'sess-001',
    name: 'Refactor authentication module',
    created_at: '2026-07-18T15:30:00Z',
    template_name: 'default',
    workspace: '~/projects/wing-agent',
    last_interaction: '2026-07-18T16:45:00Z',
  },
  {
    id: 'sess-002',
    name: 'Fix WebSocket reconnection logic',
    created_at: '2026-07-18T10:00:00Z',
    template_name: 'default',
    workspace: '~/projects/wing-agent',
    last_interaction: '2026-07-18T14:20:00Z',
  },
  {
    id: 'sess-003',
    name: 'Design system tokens',
    created_at: '2026-07-17T09:00:00Z',
    template_name: 'default',
    workspace: '~/projects/wing-ui',
    last_interaction: '2026-07-17T18:30:00Z',
  },
  {
    id: 'sess-004',
    name: 'API endpoint documentation',
    created_at: '2026-07-16T14:00:00Z',
    template_name: 'default',
    workspace: '~/projects/wing-agent',
    last_interaction: '2026-07-16T16:00:00Z',
  },
  {
    id: 'sess-005',
    name: 'Performance profiling session',
    created_at: '2026-07-15T11:00:00Z',
    template_name: 'default',
    workspace: '~/projects/wing-agent',
    last_interaction: '2026-07-15T13:45:00Z',
  },
  {
    id: 'sess-006',
    name: 'Unit tests for event bus',
    created_at: '2026-07-14T08:00:00Z',
    template_name: 'default',
    workspace: '~/projects/wing-agent',
    last_interaction: '2026-07-14T12:00:00Z',
  },
]
