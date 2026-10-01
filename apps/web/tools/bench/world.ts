/**
 * Benchmark world generator: 1000+ cells for long-session performance testing.
 */

import type { World, WorldSession } from '../shot/server';

const MINUTE = 60_000;

function message(
  role: string,
  content: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return { role, content, uuid: `${role}-${content.slice(0, 12)}-${Date.now()}`, ...extra };
}

/**
 * Generate a world with `count` pairs (user + assistant) of simple messages.
 *
 * Total cells = count * 2 (user + assistant) plus events.
 * Default: 500 pairs = 1000 cells.
 */
export function longSessionWorld(count = 500): World {
  const messages: Record<string, unknown>[] = [];

  for (let i = 0; i < count; i++) {
    messages.push(
      message('user', `This is user message number ${i + 1}. What do you think?`),
      message('assistant', `This is assistant response number ${i + 1}. Here is some text content.`),
    );
  }

  const session: WorldSession = {
    id: 'bench-1',
    name: `Benchmark (${count * 2} cells)`,
    workspace: '/tmp/bench',
    status: 'idle',
    lastInteraction: new Date(Date.now() - MINUTE).toISOString(),
    messages,
    events: [],
    info: {
      model: 'test-model',
      thinking: false,
      reasoningEffort: null,
      yolo: false,
      workdir: '/tmp/bench',
      usedTokens: count * 200,
      windowTokens: 200_000,
      messageCount: count * 2,
    },
  };

  return { sessions: [session] };
}
