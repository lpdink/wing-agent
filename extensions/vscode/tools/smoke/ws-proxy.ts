/**
 * Smoke-only TCP gate in front of the gateway: **no WS compression**.
 *
 * Why this exists (measured, not guessed):
 *
 * Node 25's `undici` WebSocket (the `globalThis.WebSocket` the host uses) offers
 * `permessage-deflate` and then *does not dispatch* some compressed frames that
 * the gateway really sent — the bytes leave the server (verified with a TCP tap),
 * the `message` event never fires, and the tab keeps an empty transcript until the
 * next client write. A raw Python client and an uncompressed handshake both
 * receive the very same frames immediately, so the payload and the gateway are
 * fine; the stall is in the client's WS stack. See `06_integration/design.md`
 * ("发现的问题" F5) for the measurements.
 *
 * The smoke must be deterministic and it drives the real host from Node, so it
 * removes the variable: this relay rewrites the client's upgrade request and drops
 * `Sec-WebSocket-Extensions`, which makes the gateway answer with plain frames.
 * Plain HTTP passes through untouched (health probe, session RPCs, …).
 *
 * Deliberately dependency-free: a TCP relay plus one header rewrite. It is only
 * ever used by `tools/smoke`; production talks to the gateway directly.
 */

import net from 'node:net';
import type { AddressInfo, Server, Socket } from 'node:net';

export interface NoCompressionProxy {
  readonly port: number;
  stop(): Promise<void>;
}

/**
 * Start the relay.
 *
 * `targetPort` is the smoke gateway's port; the returned `port` is what the host
 * must talk to (WebSocket *and* HTTP).
 */
export function startNoCompressionProxy(
  targetPort: number,
  options: { readonly stripExtensions?: boolean } = {},
): Promise<NoCompressionProxy> {
  const strip = options.stripExtensions ?? true;
  const sockets = new Set<Socket>();
  const server: Server = net.createServer((client: Socket) => {
    const upstream = net.connect({ port: targetPort, host: '127.0.0.1' });
    sockets.add(client);
    sockets.add(upstream);
    let handshakeSeen = false;
    client.on('data', (chunk: Buffer) => {
      const payload = handshakeSeen || !strip ? chunk : rewriteHandshake(chunk);
      handshakeSeen = true;
      upstream.write(payload);
    });
    upstream.on('data', (chunk: Buffer) => {
      client.write(chunk);
    });
    const close = (): void => {
      sockets.delete(client);
      sockets.delete(upstream);
      client.destroy();
      upstream.destroy();
    };
    client.on('end', close);
    client.on('error', close);
    upstream.on('end', close);
    upstream.on('error', close);
  });

  return new Promise<NoCompressionProxy>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const port = (server.address() as AddressInfo).port;
      resolve({
        port,
        stop: () =>
          new Promise<void>((done) => {
            for (const socket of sockets) {
              socket.destroy();
            }
            server.close(() => {
              done();
            });
          }),
      });
    });
  });
}

/** Drop the `Sec-WebSocket-Extensions` line from the first chunk of a connection. */
function rewriteHandshake(chunk: Buffer): Buffer {
  const text = chunk.toString('latin1');
  const end = text.indexOf('\r\n\r\n');
  if (end < 0) {
    return chunk;
  }
  const head = text
    .slice(0, end)
    .split('\r\n')
    .filter((line) => !/^sec-websocket-extensions:/i.test(line))
    .join('\r\n');
  return Buffer.from(`${head}${text.slice(end)}`, 'latin1');
}
