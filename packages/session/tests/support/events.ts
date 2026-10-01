/**
 * Event and material factories for the package's own tests.
 *
 * The gateway is not part of the picture here: the reduction lane's input is a
 * decoded `WingEvent` (or a `sync_session` payload), so the tests build exactly
 * those — no socket, no HTTP, no host. Field sets mirror what
 * `@wing-agent/client` decodes from the wire (`packages/client/src/protocol/`).
 */

import type {
  AskEvent,
  AskQuestion,
  DiffContentEvent,
  DoneEvent,
  ErrorEvent,
  ContextStatsEvent,
  InterruptedEvent,
  LlmCallMetricsEvent,
  NoticeEvent,
  ReasoningEvent,
  SessionMessage,
  SessionStateChangedEvent,
  SyncSessionEvent,
  TextEvent,
  ToolCallEvent,
  ToolCallResultEvent,
  ToolCallStreamEvent,
  TurnResultEvent,
  TurnStartedEvent,
  UserMessageAcceptedEvent,
} from '@wing-agent/client';

import type { CellModel } from '../../src';

/** The session every factory defaults to. */
export const SESSION = 's-1';

let counter = 0;

/** `EventMeta` without the discriminant-specific fields. */
function meta(sessionId: string = SESSION): {
  readonly created_at: string;
  readonly request_id: string;
  readonly session_id: string;
  readonly uuid: null;
} {
  counter += 1;
  return {
    created_at: '2026-09-18T08:00:00.000',
    request_id: `req-${counter}`,
    session_id: sessionId,
    uuid: null,
  };
}

export function turnStarted(sessionId: string = SESSION): TurnStartedEvent {
  return { ...meta(sessionId), type: 'turn_started' };
}

export function text(content: string, sessionId: string = SESSION): TextEvent {
  return { ...meta(sessionId), type: 'text', content };
}

export function reasoning(content: string, sessionId: string = SESSION): ReasoningEvent {
  return { ...meta(sessionId), type: 'reasoning', content };
}

export function toolCallStream(input: {
  readonly toolCallId: string;
  readonly toolName: string;
  readonly fragment: string;
  readonly isFinal?: boolean;
  readonly sessionId?: string;
}): ToolCallStreamEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'tool_call_stream',
    tool_call_id: input.toolCallId,
    tool_name: input.toolName,
    args_fragment: input.fragment,
    is_final: input.isFinal ?? false,
  };
}

export function toolCall(input: {
  readonly toolCallId: string;
  readonly toolName: string;
  readonly toolArgs: Record<string, unknown>;
  readonly sessionId?: string;
}): ToolCallEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'tool_call',
    tool_call_id: input.toolCallId,
    tool_name: input.toolName,
    tool_args: input.toolArgs as ToolCallEvent['tool_args'],
  };
}

export function toolCallResult(input: {
  readonly toolCallId: string;
  readonly toolName: string;
  readonly toolArgs?: Record<string, unknown>;
  readonly toolResult: string;
  readonly toolSuccess: boolean;
  readonly sessionId?: string;
}): ToolCallResultEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'tool_call_result',
    tool_call_id: input.toolCallId,
    tool_name: input.toolName,
    tool_args: (input.toolArgs ?? {}) as ToolCallResultEvent['tool_args'],
    tool_result: input.toolResult,
    tool_success: input.toolSuccess,
    model: 'test-model',
  };
}

export function diffContent(input: {
  readonly toolCallId: string;
  readonly path: string;
  readonly oldText: string | null;
  readonly newText: string;
  readonly oldStartLine?: number;
  readonly newStartLine?: number;
  readonly sessionId?: string;
}): DiffContentEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'diff_content',
    tool_call_id: input.toolCallId,
    path: input.path,
    old_text: input.oldText,
    new_text: input.newText,
    old_start_line: input.oldStartLine ?? 1,
    new_start_line: input.newStartLine ?? 1,
  };
}

export function askEvent(input: {
  readonly toolCallId: string;
  readonly questions?: readonly AskQuestion[];
  readonly question?: string;
  readonly choices?: readonly string[];
  readonly required?: boolean;
  readonly sessionId?: string;
}): AskEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'ask',
    tool_call_id: input.toolCallId,
    questions: input.questions ?? [],
    question: input.question ?? '',
    choices: input.choices ?? [],
    required: input.required ?? false,
  };
}

export function done(sessionId: string = SESSION): DoneEvent {
  return { ...meta(sessionId), type: 'done' };
}

export function interrupted(sessionId: string = SESSION): InterruptedEvent {
  return { ...meta(sessionId), type: 'interrupted' };
}

export function errorEvent(message: string, sessionId: string = SESSION): ErrorEvent {
  return {
    ...meta(sessionId),
    type: 'error',
    status_code: 500,
    message,
    error_code: null,
    detail: null,
  };
}

export function notice(message: string, sessionId: string = SESSION): NoticeEvent {
  return {
    ...meta(sessionId),
    type: 'notice',
    level: 'warning',
    message,
    attempt: null,
    max_attempts: null,
    retry_in_s: null,
  };
}

export function userMessageAccepted(
  requestId: string,
  sessionId: string = SESSION,
): UserMessageAcceptedEvent {
  return {
    ...meta(sessionId),
    type: 'user_message_accepted',
    content: 'hi',
    origin_request_id: requestId,
  };
}

export function llmCallMetrics(input: {
  readonly promptTokens?: number;
  readonly completionTokens?: number;
  readonly cachedTokens?: number;
  readonly sessionId?: string;
}): LlmCallMetricsEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'llm_call_metrics',
    model: 'test-model',
    prompt_tokens: input.promptTokens ?? 100,
    completion_tokens: input.completionTokens ?? 20,
    cached_tokens: input.cachedTokens ?? 0,
    first_chunk_rt_ms: 120,
    tokens_per_sec: 42.5,
    stop_reason: 'end_turn',
  };
}

export function contextStats(input: {
  readonly messageCount?: number;
  readonly totalTokens?: number;
  readonly windowTokens?: number;
  readonly sessionId?: string;
}): ContextStatsEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'context_stats',
    message_count: input.messageCount ?? 3,
    total_tokens: input.totalTokens ?? 1_234,
    context_window_tokens: input.windowTokens ?? 200_000,
    system_prompt_parts: [],
  };
}

export function turnResult(input: {
  readonly subtype?: string;
  readonly isError?: boolean;
  readonly result?: string | null;
  readonly sessionId?: string;
}): TurnResultEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'turn_result',
    subtype: input.subtype ?? 'success',
    is_error: input.isError ?? false,
    result: input.result ?? 'all good',
    num_turns: 1,
    duration_ms: 1_500,
    usage: { input_tokens: 100, output_tokens: 20 },
    errors: [],
  };
}

export function stateChanged(
  fields: Partial<Omit<SessionStateChangedEvent, 'type'>>,
  sessionId: string = SESSION,
): SessionStateChangedEvent {
  return {
    ...meta(sessionId),
    type: 'session_state_changed',
    model: null,
    thinking: null,
    reasoning_effort: null,
    yolo: null,
    title: null,
    agent: null,
    ...fields,
  };
}

export function message(input: {
  readonly role: string;
  readonly content: string;
  readonly reasoning?: string | null;
  readonly toolCalls?: readonly { readonly id: string; readonly name: string; readonly arguments: unknown }[];
  readonly toolCallId?: string | null;
  readonly uuid?: string | null;
}): SessionMessage {
  return {
    role: input.role,
    content: input.content,
    uuid: input.uuid ?? null,
    reasoning_content: input.reasoning ?? null,
    tool_calls: (input.toolCalls ?? []).map((call) => ({
      id: call.id,
      name: call.name,
      arguments: call.arguments as SessionMessage['tool_calls'][number]['arguments'],
    })),
    tool_call_id: input.toolCallId ?? null,
  };
}

/** A `sync_session` snapshot with everything the caller did not state defaulted. */
export function syncSession(input: {
  readonly messages?: readonly SessionMessage[];
  readonly uncommitted?: SessionMessage | null;
  readonly uncommittedTools?: readonly {
    readonly tool_call_id: string;
    readonly tool_name: string;
    readonly args_fragment: string;
  }[];
  readonly events?: readonly SyncSessionEvent['events'][number][];
  readonly status?: SyncSessionEvent['status'];
  readonly turnStartedAt?: string | null;
  readonly agent?: SyncSessionEvent['agent'];
  readonly name?: string | null;
  readonly draft?: string | null;
  readonly sessionId?: string;
}): SyncSessionEvent {
  return {
    ...meta(input.sessionId ?? SESSION),
    type: 'sync_session',
    session_id: input.sessionId ?? SESSION,
    messages: input.messages ?? [],
    uncommitted: input.uncommitted ?? null,
    uncommitted_tools: input.uncommittedTools ?? [],
    events: input.events ?? [],
    status: input.status ?? 'idle',
    turn_started_at: input.turnStartedAt ?? null,
    agent: input.agent ?? null,
    name: input.name ?? null,
    draft: input.draft ?? null,
  };
}

/**
 * Compare two transcripts on everything that is *content*.
 *
 * Cell ids and every wall-clock field are dropped: they are per-path by nature (a
 * replayed tool call has no measured `startedAt`, a replayed thinking block has no
 * measured duration). Anything else differing is a replay ≠ live bug.
 */
export function stableCells(cells: readonly CellModel[]): unknown[] {
  return cells.map((cell, index) => {
    const copy: Record<string, unknown> = { ...cell, id: `#${index}` };
    if ('createdAt' in copy) {
      copy['createdAt'] = 0;
    }
    if ('startedAt' in copy) {
      copy['startedAt'] = null;
      copy['finishedAt'] = null;
    }
    if ('durationMs' in copy) {
      copy['durationMs'] = null;
    }
    return copy;
  });
}

/** Cell kinds, in order — the compact form of an ordering assertion. */
export function kinds(cells: readonly CellModel[]): string[] {
  return cells.map((cell) => cell.kind);
}
