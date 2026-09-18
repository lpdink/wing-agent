/**
 * The smoke's fake model provider — OpenAI-compatible, scripted, deterministic.
 *
 * Same idea as `libs/wing-probe/wing_probe/provider/**` (a script is a list of
 * `Turn`s consumed one per request, routed by model name, tool arguments can be
 * cut mid-JSON), re-implemented in Node because the smoke drives a Node front end
 * and must not import `wing`.
 *
 * Wire surface (only what the gateway's `openai` provider actually calls):
 *
 * - `POST /v1/chat/completions` — `stream:true` answers SSE in the exact frame
 *   order the Python probe uses: role frame → thinking deltas →
 *   content deltas → tool-call deltas → finish_reason frame → usage frame →
 *   `[DONE]`. `stream:false` answers a complete `chat.completion` (compaction
 *   calls are non-streaming).
 * - `GET /v1/models` — the registered model names (the gateway's dynamic model
 *   discovery hits this).
 *
 * Every request is logged (`requests`), so a scenario can assert "this command
 * never reached the model" as a *count*, not as an absence of side effects.
 */

import { createServer } from 'node:http';
import type { IncomingMessage, Server, ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';

export interface Usage {
  readonly promptTokens?: number;
  readonly completionTokens?: number;
  readonly cachedTokens?: number;
}

export interface ToolCallSpec {
  readonly name: string;
  /** Object → serialized as JSON; string → used as-is (can be invalid JSON). */
  readonly args: unknown;
  readonly id?: string;
  /** Character offsets where the streamed `arguments` string is cut. */
  readonly cut?: number | readonly number[];
}

export interface Turn {
  readonly thinking?: string;
  readonly text?: string;
  readonly toolCalls?: readonly ToolCallSpec[];
  readonly usage?: Usage;
  readonly finish?: string;
  /** Chunk size for thinking/text (characters). */
  readonly chunk?: number;
  /** Delay before every chunk except the first (ms). */
  readonly delayMs?: number;
}

export interface LoggedRequest {
  readonly model: string;
  readonly stream: boolean;
  readonly index: number;
  /** Number of messages in the request (a crude "did the context grow" probe). */
  readonly messageCount: number;
  readonly body: Record<string, unknown>;
}

interface RegisteredScript {
  readonly turns: Turn[];
  consumed: number;
}

export class ScriptError extends Error {}

/** `call_<turn>_<index>` — deterministic across runs (probe's rule). */
export function callId(turnIndex: number, callIndex: number): string {
  return `call_${turnIndex}_${callIndex}`;
}

function argumentText(call: ToolCallSpec): string {
  if (typeof call.args === 'string') {
    return call.args;
  }
  return JSON.stringify(call.args);
}

function cutPoints(call: ToolCallSpec): readonly number[] {
  const cut = call.cut;
  if (cut === undefined) {
    return [];
  }
  return typeof cut === 'number' ? [cut] : [...cut];
}

function argumentChunks(call: ToolCallSpec): readonly string[] {
  const text = argumentText(call);
  const raw = cutPoints(call);
  if (raw.length === 0) {
    return [text];
  }
  const bounds = [0, ...raw, text.length];
  const parts: string[] = [];
  for (let index = 0; index < bounds.length - 1; index += 1) {
    parts.push(text.slice(bounds[index], bounds[index + 1]));
  }
  return parts;
}

function splitText(text: string | undefined, chunk: number | undefined): readonly string[] {
  if (text === undefined || text === '') {
    return [];
  }
  if (chunk === undefined || chunk >= text.length) {
    return [text];
  }
  const parts: string[] = [];
  for (let index = 0; index < text.length; index += chunk) {
    parts.push(text.slice(index, index + chunk));
  }
  return parts;
}

function usageWire(usage: Usage | undefined): Record<string, unknown> {
  const prompt = usage?.promptTokens ?? 0;
  const completion = usage?.completionTokens ?? 0;
  return {
    prompt_tokens: prompt,
    completion_tokens: completion,
    total_tokens: prompt + completion,
    prompt_tokens_details: { cached_tokens: usage?.cachedTokens ?? 0 },
  };
}

export class FakeProvider {
  private readonly scripts = new Map<string, RegisteredScript>();
  private server: Server | null = null;
  private port = 0;
  private readonly startedAt = Date.now();

  readonly requests: LoggedRequest[] = [];

  // ── lifecycle ───────────────────────────────────────────────────────

  async start(): Promise<void> {
    if (this.server !== null) {
      return;
    }
    const server = createServer((request, response) => {
      void this.handle(request, response);
    });
    this.server = server;
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(0, '127.0.0.1', () => {
        resolve();
      });
    });
    this.port = (server.address() as AddressInfo).port;
  }

  async stop(): Promise<void> {
    const server = this.server;
    this.server = null;
    if (server === null) {
      return;
    }
    await new Promise<void>((resolve) => {
      server.close(() => {
        resolve();
      });
      server.closeAllConnections();
    });
  }

  get url(): string {
    return `http://127.0.0.1:${this.port}`;
  }

  /** The OpenAI-compatible root the gateway's provider config points at. */
  get baseUrl(): string {
    return `${this.url}/v1`;
  }

  // ── scripts ─────────────────────────────────────────────────────────

  /** Replace the script of one model (a scenario resets before it runs). */
  setScript(model: string, turns: readonly Turn[]): void {
    if (turns.length === 0) {
      throw new Error(`script for ${model} needs at least one turn`);
    }
    this.scripts.set(model, { turns: [...turns], consumed: 0 });
  }

  models(): readonly string[] {
    return [...this.scripts.keys()].sort();
  }

  /** Request count per model (assert "the model was never called"). */
  requestCount(): number {
    return this.requests.length;
  }

  private consume(model: string): { turn: Turn; index: number } {
    const script = this.scripts.get(model);
    if (script === undefined) {
      throw new ScriptError(
        `model ${JSON.stringify(model)} has no script (registered: ${this.models().join(', ') || '<none>'})`,
      );
    }
    if (script.consumed >= script.turns.length) {
      throw new ScriptError(
        `script for ${JSON.stringify(model)} is exhausted: consumed ${script.consumed}/${script.turns.length} turns`,
      );
    }
    const index = script.consumed;
    script.consumed += 1;
    return { turn: script.turns[index] as Turn, index };
  }

  // ── HTTP ────────────────────────────────────────────────────────────

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const url = new URL(request.url ?? '/', this.url);
    if (request.method === 'GET' && url.pathname === '/v1/models') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(
        JSON.stringify({
          object: 'list',
          data: this.models().map((id) => ({
            id,
            object: 'model',
            created: Math.floor(this.startedAt / 1000),
            owned_by: 'wing-vscode-smoke',
          })),
        }),
      );
      return;
    }
    if (request.method !== 'POST' || url.pathname !== '/v1/chat/completions') {
      response.writeHead(404, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { message: `no fake route for ${url.pathname}` } }));
      return;
    }

    const body = await readJson(request);
    const model = typeof body['model'] === 'string' ? body['model'] : '';
    const stream = body['stream'] === true;
    const messages = Array.isArray(body['messages']) ? body['messages'] : [];
    this.requests.push({
      model,
      stream,
      index: this.requests.length,
      messageCount: messages.length,
      body,
    });

    let turn: Turn;
    let index: number;
    try {
      ({ turn, index } = this.consume(model));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      response.writeHead(500, { 'content-type': 'application/json' });
      response.end(
        JSON.stringify({ error: { message: `${message}\n${this.describe()}`, type: 'probe_error' } }),
      );
      return;
    }

    const completionId = `chatcmpl-smoke-${this.requests.length}`;
    const created = Math.floor(Date.now() / 1000);
    if (!stream) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify(this.completion(turn, index, model, completionId, created)));
      return;
    }
    await this.streamTurn(response, turn, index, model, completionId, created);
  }

  private describe(): string {
    return [...this.scripts.entries()]
      .map(([model, script]) => `  ${model}: ${script.consumed}/${script.turns.length} turns consumed`)
      .join('\n');
  }

  private completion(
    turn: Turn,
    index: number,
    model: string,
    id: string,
    created: number,
  ): Record<string, unknown> {
    const message: Record<string, unknown> = { role: 'assistant', content: turn.text ?? '' };
    if (turn.thinking !== undefined) {
      message['reasoning_content'] = turn.thinking;
    }
    if (turn.toolCalls !== undefined && turn.toolCalls.length > 0) {
      message['tool_calls'] = turn.toolCalls.map((call, callIndex) => ({
        id: call.id ?? callId(index, callIndex),
        type: 'function',
        function: { name: call.name, arguments: argumentText(call) },
      }));
    }
    return {
      id,
      object: 'chat.completion',
      created,
      model,
      choices: [
        {
          index: 0,
          message,
          finish_reason:
            turn.finish ??
            (turn.toolCalls !== undefined && turn.toolCalls.length > 0 ? 'tool_calls' : 'stop'),
        },
      ],
      usage: usageWire(turn.usage),
    };
  }

  /** One SSE frame (`data: {json}\n\n`) or the terminal `[DONE]`. */
  private frame(payload: Record<string, unknown> | null): string {
    if (payload === null) {
      return 'data: [DONE]\n\n';
    }
    return `data: ${JSON.stringify(payload)}\n\n`;
  }

  private chunk(
    id: string,
    created: number,
    model: string,
    delta: Record<string, unknown> | null,
    finishReason: string | null,
    usage: Usage | undefined,
  ): Record<string, unknown> {
    const payload: Record<string, unknown> = {
      id,
      object: 'chat.completion.chunk',
      created,
      model,
      choices: [],
    };
    if (usage !== undefined) {
      payload['usage'] = usageWire(usage);
      return payload;
    }
    payload['choices'] = [{ index: 0, delta: delta ?? {}, finish_reason: finishReason }];
    return payload;
  }

  private async streamTurn(
    response: ServerResponse,
    turn: Turn,
    index: number,
    model: string,
    id: string,
    created: number,
  ): Promise<void> {
    response.writeHead(200, {
      'content-type': 'text/event-stream; charset=utf-8',
      'cache-control': 'no-cache',
    });

    const payloads: Record<string, unknown>[] = [
      this.chunk(id, created, model, { role: 'assistant', content: '' }, null, undefined),
    ];
    for (const piece of splitText(turn.thinking, turn.chunk)) {
      payloads.push(this.chunk(id, created, model, { reasoning_content: piece }, null, undefined));
    }
    for (const piece of splitText(turn.text, turn.chunk)) {
      payloads.push(this.chunk(id, created, model, { content: piece }, null, undefined));
    }
    (turn.toolCalls ?? []).forEach((call, callIndex) => {
      argumentChunks(call).forEach((piece, pieceIndex) => {
        const entry: Record<string, unknown> = { index: callIndex, function: { arguments: piece } };
        if (pieceIndex === 0) {
          entry['id'] = call.id ?? callId(index, callIndex);
          entry['type'] = 'function';
          entry['function'] = { name: call.name, arguments: piece };
        }
        payloads.push(this.chunk(id, created, model, { tool_calls: [entry] }, null, undefined));
      });
    });
    const finish =
      turn.finish ?? (turn.toolCalls !== undefined && turn.toolCalls.length > 0 ? 'tool_calls' : 'stop');
    payloads.push(this.chunk(id, created, model, {}, finish, undefined));
    payloads.push(this.chunk(id, created, model, null, null, turn.usage ?? {}));

    const delayMs = turn.delayMs ?? 0;
    // A client-side abort (interrupt) closes the socket; the write then fails and
    // the turn simply stops — that is the expected path, not an error.
    const write = async (text: string, wait: boolean): Promise<boolean> => {
      if (wait && delayMs > 0) {
        await new Promise<void>((resolve) => {
          setTimeout(resolve, delayMs);
        });
      }
      return response.write(text);
    };
    try {
      for (const [payloadIndex, payload] of payloads.entries()) {
        if (!(await write(this.frame(payload), payloadIndex > 0))) {
          return;
        }
      }
      await write(this.frame(null), delayMs > 0);
    } catch {
      // Socket gone (abort) — nothing to report.
    } finally {
      response.end();
    }
  }
}

function readJson(request: IncomingMessage): Promise<Record<string, unknown>> {
  return new Promise((resolve, reject) => {
    let text = '';
    request.setEncoding('utf8');
    request.on('data', (chunk: string) => {
      text += chunk;
    });
    request.on('end', () => {
      try {
        const parsed: unknown = JSON.parse(text === '' ? '{}' : text);
        resolve(typeof parsed === 'object' && parsed !== null ? (parsed as Record<string, unknown>) : {});
      } catch (error) {
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
    request.on('error', reject);
  });
}
