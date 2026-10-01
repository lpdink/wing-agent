import { describe, expect, it } from 'vitest';

import { FORBIDDEN_PORT, assertPortAllowed, startFixtureServer, type World } from '../tools/shot/server';

/**
 * The fixture gateway's own contracts.
 *
 * It is a *test double*, but the scenes' reproducibility rests on it: a frozen live
 * script, an image endpoint that answers real bytes, and never the user's gateway
 * port. These are the parts that a scene cannot assert (a screenshot proves what the
 * page looks like, not that the fixture answered 404 where it should).
 */

describe('assertPortAllowed', () => {
  it('refuses the user’s gateway port', () => {
    expect(FORBIDDEN_PORT).toBe(32_523);
    expect(() => {
      assertPortAllowed(FORBIDDEN_PORT);
    }).toThrow(/refusing to bind/);
  });

  it('allows every other port', () => {
    for (const port of [1, 80, 5_173, 32_522, 32_524, 65_535]) {
      expect(() => {
        assertPortAllowed(port);
      }).not.toThrow();
    }
  });
});

/** A world with one session and a two-step live script (`distDir` is never read here). */
function world(): World {
  return {
    sessions: [
      {
        id: 'wing-1',
        name: 'Demo',
        workspace: '/tmp/ws',
        status: 'idle',
        lastInteraction: null,
        messages: [],
        events: [],
        live: [
          { delayMs: 5, event: { type: 'notice', session_id: 'wing-1', message: 'first' } },
          { delayMs: 5, event: { type: 'notice', session_id: 'wing-1', message: 'second' } },
        ],
        info: {
          model: 'test-model',
          thinking: false,
          reasoningEffort: null,
          yolo: false,
          workdir: '/tmp/ws',
          usedTokens: 0,
          windowTokens: 200_000,
          messageCount: 0,
        },
      },
    ],
  };
}

/** Start a fixture server on an OS-assigned port; `distDir` is only used by static hits. */
async function start(w: World = world()): Promise<Awaited<ReturnType<typeof startFixtureServer>>> {
  return startFixtureServer({ distDir: '/nonexistent-dist-for-api-tests', world: w });
}

describe('fixture workspace image endpoint', () => {
  it('answers the chart as PNG bytes', async () => {
    const server = await start();
    try {
      const response = await fetch(
        `${server.url}/api/workspace/image?session_id=wing-1&path=assets%2Fchart.png`,
      );
      expect(response.status).toBe(200);
      expect(response.headers.get('content-type')).toBe('image/png');
      const bytes = new Uint8Array(await response.arrayBuffer());
      // The PNG signature: the scene screenshots a *decoded* image, so the bytes
      // have to be a real PNG, not a placeholder string.
      expect([...bytes.slice(0, 8)]).toEqual([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
      expect(bytes.byteLength).toBeGreaterThan(200);
    } finally {
      await server.close();
    }
  });

  it('refuses an unknown session and a non-image path', async () => {
    const server = await start();
    try {
      const unknown = await fetch(`${server.url}/api/workspace/image?session_id=nope&path=a.png`);
      expect(unknown.status).toBe(404);
      const notImage = await fetch(
        `${server.url}/api/workspace/image?session_id=wing-1&path=docs%2Freadme.md`,
      );
      expect(notImage.status).toBe(404);
      const noPath = await fetch(`${server.url}/api/workspace/image?session_id=wing-1&path=`);
      expect(noPath.status).toBe(404);
    } finally {
      await server.close();
    }
  });
});

describe('fixture live script', () => {
  it('pushes the scripted events after the subscribe replay, in order', async () => {
    const server = await start();
    // `WebSocket` here is the platform global (undici in Node ≥ 22); the DOM lib
    // types the same surface — the fixture is a dev tool, not browser code.
    const socket = new WebSocket(`${server.url.replace('http', 'ws')}/ws`);
    const frames: Record<string, unknown>[] = [];
    socket.addEventListener('message', (event: MessageEvent) => {
      frames.push(JSON.parse(String(event.data)) as Record<string, unknown>);
    });
    try {
      await new Promise<void>((resolve, reject) => {
        socket.addEventListener('open', () => {
          resolve();
        });
        socket.addEventListener('error', () => {
          reject(new Error('the fixture socket refused to open'));
        });
      });

      const subscribed = await fetch(`${server.url}/api/session/subscribe`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ session_id: 'wing-1', client_id: 'c-1' }),
      });
      expect(subscribed.status).toBe(200);

      // Wait for the *last* scripted step, not for "some frame arrived": the steps
      // are timed, and a scene's readiness predicate leans on exactly this.
      await waitFor(() => frames.some((frame) => frame['message'] === 'second'));
      expect(frames[0]?.['type']).toBe('connected');
      expect(frames[1]?.['type']).toBe('sync_session');
      expect(frames.slice(2).map((frame) => frame['message'])).toEqual(['first', 'second']);
    } finally {
      socket.close();
      await server.close();
    }
  });
});

/** Poll until `predicate` holds (the live script is timer-driven). */
async function waitFor(predicate: () => boolean, timeoutMs = 2_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error('timed out waiting for the fixture');
    }
    await new Promise((resolve) => {
      setTimeout(resolve, 10);
    });
  }
}
