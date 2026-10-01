import { describe, expect, it, vi } from 'vitest';

import { workspaceImagePath } from '../src/images/policy';
import { createImageResolver, type ImageBytes, type ImagePlatform } from '../src/images/resolver';
import { imageTarget } from '../src/images/target';
import { workspaceImageUrl } from '../src/images/url';
import { DEFAULT_SETTINGS, type GatewaySettings } from '../src/settings/settings';

/**
 * The image adapter (step 08's `apps/web` side of step 03's endpoint).
 *
 * Everything here is framework-free and platform-free: the resolver takes injected
 * bytes and an injected encoder, so these tests assert the three states (never asked
 * / loadable / refused) and the cache's lifetime rules without a DOM, a gateway or a
 * browser.
 */

/** A platform that answers with fixed bytes and records every call. */
function fakePlatform(
  options: { readonly bytes?: ImageBytes | null; readonly deny?: readonly string[] } = {},
): {
  readonly platform: ImagePlatform;
  readonly loads: { url: string; apiKey: string | null }[];
  readonly released: string[];
  readonly encodes: ImageBytes[];
} {
  const loads: { url: string; apiKey: string | null }[] = [];
  const released: string[] = [];
  const encodes: ImageBytes[] = [];
  const answer =
    options.bytes === undefined
      ? { bytes: new Uint8Array([1, 2, 3]), contentType: 'image/png' }
      : options.bytes;
  let seq = 0;
  return {
    loads,
    released,
    encodes,
    platform: {
      load: (url, { apiKey }) => {
        loads.push({ url, apiKey });
        const path = new URL(url).searchParams.get('path') ?? '';
        if (options.deny?.some((denied) => path.endsWith(denied)) === true) {
          return Promise.resolve(null);
        }
        return Promise.resolve(answer);
      },
      encode: (image) => {
        encodes.push(image);
        seq += 1;
        return `blob:image-${seq}`;
      },
      release: (uri) => {
        released.push(uri);
      },
    },
  };
}

const TARGET = { baseUrl: 'http://gw.lan:32523', apiKey: null, sessionId: 'session-1' };

describe('workspace image policy', () => {
  it('accepts the paths a model writes', () => {
    for (const src of [
      'assets/chart.png',
      './docs/diagram.svg',
      '/Users/dev/projects/wing/shot.jpeg',
      'shot.PNG',
      'nested/dir/file.apng',
      'my%20image.png',
      'a+b.png',
      '../outside.png', // the gateway owns the boundary; the pre-filter must not guess it
    ]) {
      expect(workspaceImagePath(src), src).not.toBeNull();
    }
  });

  it('decodes the source once, like the destination markdown-it hands over', () => {
    expect(workspaceImagePath('  my%20image.png  ')).toBe('my image.png');
    expect(workspaceImagePath('50%.png')).toBe('50%.png'); // malformed escape stays verbatim
  });

  it('refuses everything that cannot be a workspace file', () => {
    for (const src of [
      '',
      '   ',
      'https://example.com/a.png',
      'data:image/png;base64,AAAA',
      'blob:http://localhost/abc',
      'notes.txt',
      'archive.tar.gz',
      '.png', // a dotfile has no extension
      'trailing.',
      `long${'a'.repeat(1_024)}.png`,
      'bad\u0007.png',
    ]) {
      expect(workspaceImagePath(src), JSON.stringify(src)).toBeNull();
    }
  });

  it('treats a Windows path as a path, not as a scheme', () => {
    expect(workspaceImagePath('C:\\tmp\\shot.png')).toBe('C:\\tmp\\shot.png');
    expect(workspaceImagePath('C:%5Ctmp%5Cshot.png')).toBe('C:\\tmp\\shot.png');
  });
});

describe('workspace image URL', () => {
  it('builds the frozen endpoint shape', () => {
    expect(workspaceImageUrl('http://127.0.0.1:32523', 'session-1', 'assets/chart.png')).toBe(
      'http://127.0.0.1:32523/api/workspace/image?session_id=session-1&path=assets%2Fchart.png',
    );
  });

  it('normalises a trailing slash and encodes the path unambiguously', () => {
    expect(workspaceImageUrl('https://gw.lan/', 's-1', 'my image+a?b.png')).toBe(
      'https://gw.lan/api/workspace/image?session_id=s-1&path=my+image%2Ba%3Fb.png',
    );
  });
});

describe('image resolver', () => {
  it('answers a loadable source with the encoded bytes', async () => {
    const fake = fakePlatform();
    const resolver = createImageResolver({ target: () => TARGET, platform: fake.platform });

    await expect(resolver.resolve(['assets/chart.png'])).resolves.toEqual([
      { src: 'assets/chart.png', uri: 'blob:image-1' },
    ]);
    expect(fake.loads).toEqual([
      {
        url: 'http://gw.lan:32523/api/workspace/image?session_id=session-1&path=assets%2Fchart.png',
        apiKey: null,
      },
    ]);
    expect(fake.encodes).toHaveLength(1);
  });

  it('passes the API key as a header value (an <img> could not)', async () => {
    const fake = fakePlatform();
    const resolver = createImageResolver({
      target: () => ({ ...TARGET, apiKey: 'secret' }),
      platform: fake.platform,
    });

    await resolver.resolve(['a.png']);
    expect(fake.loads[0]?.apiKey).toBe('secret');
  });

  it('turns a refusal into a permanent `null` (the renderer keeps its link)', async () => {
    const fake = fakePlatform({ bytes: null });
    const resolver = createImageResolver({ target: () => TARGET, platform: fake.platform });

    await expect(resolver.resolve(['broken.png'])).resolves.toEqual([{ src: 'broken.png', uri: null }]);
    // Asked once: the answer is final, exactly like the VS Code host's.
    await expect(resolver.resolve(['broken.png'])).resolves.toEqual([{ src: 'broken.png', uri: null }]);
    expect(fake.loads).toHaveLength(1);
  });

  it('refuses a source the policy rejects without touching the network', async () => {
    const fake = fakePlatform();
    const resolver = createImageResolver({ target: () => TARGET, platform: fake.platform });

    await expect(resolver.resolve(['https://example.com/a.png'])).resolves.toEqual([
      { src: 'https://example.com/a.png', uri: null },
    ]);
    expect(fake.loads).toEqual([]);
  });

  it('answers nothing when there is no session to ask about', async () => {
    const fake = fakePlatform();
    const resolver = createImageResolver({ target: () => null, platform: fake.platform });

    // Not `null`: the source stays unknown, so the next mount asks again (which is
    // what a session switch produces).
    await expect(resolver.resolve(['a.png'])).resolves.toEqual([]);
    expect(fake.loads).toEqual([]);
  });

  it('caches by source and fetches once per in-flight source', async () => {
    const fake = fakePlatform();
    const resolver = createImageResolver({ target: () => TARGET, platform: fake.platform });

    const [first, second] = await Promise.all([resolver.resolve(['a.png']), resolver.resolve(['a.png'])]);
    expect(first).toEqual([{ src: 'a.png', uri: 'blob:image-1' }]);
    expect(second).toEqual(first);
    expect(fake.loads).toHaveLength(1);

    await resolver.resolve(['a.png']);
    expect(fake.loads).toHaveLength(1);
  });

  it('re-asks when the session changes (the cache is per workspace)', async () => {
    const fake = fakePlatform();
    let sessionId = 'session-1';
    const resolver = createImageResolver({
      target: () => ({ ...TARGET, sessionId }),
      platform: fake.platform,
    });

    await resolver.resolve(['assets/chart.png']);
    sessionId = 'session-2';
    await resolver.resolve(['assets/chart.png']);

    expect(fake.loads.map((call) => new URL(call.url).searchParams.get('session_id'))).toEqual([
      'session-1',
      'session-2',
    ]);
  });

  it('bounds the cache and releases what it evicts', async () => {
    const fake = fakePlatform();
    const resolver = createImageResolver({ target: () => TARGET, platform: fake.platform, maxEntries: 1 });

    await resolver.resolve(['a.png']);
    await resolver.resolve(['b.png']);
    expect(fake.released).toEqual(['blob:image-1']);

    resolver.clear();
    expect(fake.released).toEqual(['blob:image-1', 'blob:image-2']);
  });

  it('releases every object URL on clear(), and only the loadable ones', async () => {
    const fake = fakePlatform({ deny: ['broken.png'] });
    const resolver = createImageResolver({ target: () => TARGET, platform: fake.platform });

    await resolver.resolve(['a.png', 'broken.png']);
    resolver.clear();
    expect(fake.released).toEqual(['blob:image-1']);
  });
});

describe('image target', () => {
  const location = { origin: 'http://localhost:5173' };

  it('uses the page origin in same-origin mode', () => {
    expect(imageTarget({ settings: DEFAULT_SETTINGS, location, sessionId: 's-1' })).toEqual({
      baseUrl: 'http://localhost:5173',
      apiKey: null,
      sessionId: 's-1',
    });
  });

  it('follows an explicit address and its key', () => {
    const settings: GatewaySettings = {
      ...DEFAULT_SETTINGS,
      host: 'gw.lan',
      port: 8443,
      scheme: 'https',
      apiKey: 'secret',
    };
    expect(imageTarget({ settings, location, sessionId: 's-1' })).toEqual({
      baseUrl: 'https://gw.lan:8443',
      apiKey: 'secret',
      sessionId: 's-1',
    });
  });

  it('answers null instead of throwing on an address the settings cannot produce', () => {
    const settings: GatewaySettings = { ...DEFAULT_SETTINGS, host: 'gw.lan:32523' };
    const warn = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    expect(imageTarget({ settings, location, sessionId: 's-1' })).toBeNull();
    warn.mockRestore();
  });

  it('answers null without a session', () => {
    expect(imageTarget({ settings: DEFAULT_SETTINGS, location, sessionId: null })).toBeNull();
  });
});
