/**
 * Nested structures shared by the event mirror and the HTTP mirror.
 *
 * These live in their own module so `events.ts` and `http.ts` can both use them
 * without importing each other (only `errors.ts` reads from `http.ts`, and only
 * for the `ErrorResponse` type).
 *
 * Mirrors: `wing/event/base.py::AgentInfo`, `wing/event/query_response.py::
 * BranchTargetInfo`, and the `ask` question structure emitted by
 * `wing/tools/ask_user.py` (`AskEvent.questions` is typed `list[dict]` on the
 * backend, so decoding is tolerant by design).
 */

import {
  type JsonObject,
  booleanOr,
  decodeEach,
  isJsonObject,
  optString,
  readJsonArray,
  readStringArray,
  stringOr,
} from './json';

/** `wing/event/base.py::AgentInfo` — the agent configuration snapshot. */
export interface AgentInfo {
  readonly model_name: string;
  readonly system_prompt: string | null;
  readonly tools: readonly string[];
  readonly skills: readonly string[];
  readonly rules: readonly string[];
  readonly workspace: string | null;
  readonly provider_name: string | null;
}

/** `wing/event/query_response.py::BranchTargetInfo`. */
export interface BranchTarget {
  readonly uuid: string;
  readonly content: string;
  readonly role: string;
}

/** One selectable option of an Ask question. */
export interface AskOption {
  readonly label: string;
  readonly description: string;
}

/** One question of a multi-question Ask (every field defaults — see the header). */
export interface AskQuestion {
  readonly id: string;
  /** Short tab label (the UI falls back to the id). */
  readonly header: string;
  readonly question: string;
  readonly multiSelect: boolean;
  readonly options: readonly AskOption[];
  /** Legacy plain-string choices (old gateway records). */
  readonly choices: readonly string[];
}

/** Decode `AgentInfo`; `null` when the payload is not one. */
export function decodeAgentInfo(value: unknown): AgentInfo | null {
  if (!isJsonObject(value)) {
    return null;
  }
  const modelName = optString(value, 'model_name');
  if (modelName === null) {
    return null;
  }
  return {
    model_name: modelName,
    system_prompt: optString(value, 'system_prompt'),
    tools: readStringArray(value, 'tools'),
    skills: readStringArray(value, 'skills'),
    rules: readStringArray(value, 'rules'),
    workspace: optString(value, 'workspace'),
    provider_name: optString(value, 'provider_name'),
  };
}

/** Decode one branch/rewind target; `null` when it has no usable uuid. */
export function decodeBranchTarget(value: unknown): BranchTarget | null {
  if (!isJsonObject(value)) {
    return null;
  }
  const uuid = optString(value, 'uuid');
  if (uuid === null) {
    return null;
  }
  return { uuid, content: stringOr(value, 'content', ''), role: stringOr(value, 'role', 'user') };
}

/** Decode one `ask` question. */
export function decodeAskQuestion(value: unknown): AskQuestion | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    id: stringOr(value, 'id', ''),
    header: stringOr(value, 'header', ''),
    question: stringOr(value, 'question', ''),
    multiSelect: booleanOr(value, 'multiSelect', false),
    options: decodeEach(readJsonArray(value, 'options'), decodeAskOption),
    choices: readStringArray(value, 'choices'),
  };
}

function decodeAskOption(value: unknown): AskOption | null {
  if (!isJsonObject(value)) {
    return null;
  }
  const label = optString(value, 'label');
  if (label === null) {
    return null;
  }
  return { label, description: stringOr(value, 'description', '') };
}

/** Raw shape guard used by the `assistant_turn.content_blocks` mirror. */
export function asJsonObjects(values: readonly unknown[]): JsonObject[] {
  return values.filter(isJsonObject);
}
