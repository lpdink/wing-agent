import type {
  AskAnswerModel,
  AskQuestionModel,
  DiffLineModel,
  JsonValue,
  TodoItemModel,
  TodoItemStatus,
  ToolCallDisplayModel,
} from '../../shared';
import type { AskEvent, BranchTarget } from '../../core';

import { asArray, asObject } from './partial-json';

/**
 * Pure derivations: everything the host turns gateway data into before it
 * crosses the bridge.
 *
 * The webview never parses partial JSON, never diffs text and never inspects
 * wire shapes — this module is where those jobs live. Rules are ported from the
 * TUI (`crates/wing/src/ui/cells/tool_call.rs`, `diff_view.rs`,
 * `shared/panels/ask.rs`) so both frontends describe the same event the same
 * way; any deviation is called out in design.md D9.
 */

/** Placeholder the backend accepts for a skipped question (AskUserQuestion). */
export const ASK_UNANSWERED = '(user did not answer)';

// ── text helpers ──────────────────────────────────────────────────────

/**
 * Truncate by Unicode code points, appending `...` (TUI `truncate_by_chars`).
 *
 * Code points (not UTF-16 units) so CJK and emoji count the same as they do in
 * the backend's `content[:100]`.
 */
export function truncateChars(text: string, maxChars: number): string {
  const chars = Array.from(text);
  if (chars.length <= maxChars) {
    return text;
  }
  return `${chars.slice(0, Math.max(0, maxChars - 3)).join('')}...`;
}

/** Collapse a multi-line command to one line, keeping every character (`⏎`). */
export function commandOneLine(command: string): string {
  return command.trim().replace(/\r\n/g, '⏎').replace(/\n/g, '⏎');
}

/** Keep the last `n` path segments (TUI `truncate_path_segments`). */
export function truncatePathSegments(value: string, n = 6): string {
  const path = value.replace(/\/+$/, '');
  const segments = path.split('/');
  if (segments.length <= n) {
    return path;
  }
  return segments.slice(segments.length - n).join('/');
}

/**
 * Split text the way Rust's `str::lines()` does (and the backend's windowing
 * contract assumes): split on `\n`, drop one trailing empty element, strip a
 * trailing `\r` from every line.
 */
export function splitLines(text: string): string[] {
  const lines = text.split('\n');
  if (lines.length > 0 && lines[lines.length - 1] === '') {
    lines.pop();
  }
  return lines.map((line) => (line.endsWith('\r') ? line.slice(0, -1) : line));
}

// ── title ─────────────────────────────────────────────────────────────

/**
 * Tab title: explicit name → first user message → workspace basename.
 *
 * The implicit rule mirrors the backend's `session.py` auto-title
 * (`content[:100]`) and the store's `first_user_message`, so the tab shows the
 * same name the gateway would report — without waiting for a round trip.
 */
export function deriveTitle(input: {
  readonly explicit: string | null;
  readonly firstUserText: string | null;
  readonly workspace: string | null;
  readonly maxLength: number;
  readonly fallback: string;
}): string {
  if (input.explicit !== null && input.explicit.trim() !== '') {
    return input.explicit;
  }
  if (input.firstUserText !== null && input.firstUserText.trim() !== '') {
    return truncateChars(input.firstUserText.trim(), input.maxLength);
  }
  const workspace = input.workspace;
  if (workspace !== null && workspace.trim() !== '') {
    const segments = workspace.replace(/[/\\]+$/, '').split(/[/\\]/);
    const base = segments[segments.length - 1];
    if (base !== undefined && base !== '') {
      return base;
    }
  }
  return input.fallback;
}

// ── tool call display ─────────────────────────────────────────────────

const TOOL_NAMES = {
  bash: 'Bash',
  read: 'Read',
  write: 'Write',
  edit: 'Edit',
  glob: 'Glob',
  grep: 'Grep',
  askUser: 'AskUserQuestion',
  todo: 'TodoWrite',
} as const;

/** Fallback args summary for unknown tools: `key=value` pairs, truncated. */
export function fallbackArgsSummary(args: JsonValue | null): string {
  const object = asObject(args);
  if (object === null) {
    return '';
  }
  const parts: string[] = [];
  for (const [key, value] of Object.entries(object)) {
    if (key === 'purpose') {
      continue;
    }
    const rendered = typeof value === 'string' ? value : JSON.stringify(value);
    parts.push(`${key}=${truncateChars(rendered, 50)}`);
  }
  return parts.join(' ');
}

function stringField(args: Record<string, JsonValue | null>, key: string): string {
  const value = args[key];
  return typeof value === 'string' ? value : '';
}

/**
 * The collapsed tool row's presentation (`display.title` / `display.subject`).
 *
 * Ported from the TUI's `ToolRenderer::header_args`, minus the surrounding
 * parentheses and with `Read` gaining a `:offset` suffix (line 1 is implicit,
 * which is how a file reference renders clickable in the webview).
 */
export function toolDisplay(name: string, args: JsonValue | null): ToolCallDisplayModel {
  const object = asObject(args);
  const title = name === '' ? 'Tool' : name;
  if (object === null) {
    return { title, subject: '' };
  }
  switch (name) {
    case TOOL_NAMES.bash: {
      const command = commandOneLine(stringField(object, 'command'));
      return { title, subject: command };
    }
    case TOOL_NAMES.read: {
      const path = truncatePathSegments(stringField(object, 'path'));
      const offset = typeof object['offset'] === 'number' ? object['offset'] : 1;
      if (path === '') {
        return { title, subject: '' };
      }
      return { title, subject: offset > 1 ? `${path}:${offset}` : path };
    }
    case TOOL_NAMES.write:
    case TOOL_NAMES.edit: {
      const path = truncatePathSegments(stringField(object, 'path'));
      return { title, subject: path };
    }
    case TOOL_NAMES.glob:
    case TOOL_NAMES.grep: {
      const path = truncatePathSegments(stringField(object, 'path'));
      const pattern = stringField(object, 'pattern');
      const parts = [path, pattern].filter((part) => part !== '');
      return { title, subject: parts.join(', ') };
    }
    case TOOL_NAMES.todo: {
      const todos = asArray(object['todos']);
      if (todos === null || todos.length === 0) {
        return { title, subject: '' };
      }
      const open = todos.filter((item) => {
        const record = asObject(item);
        return record === null || record['status'] !== 'completed';
      }).length;
      return { title, subject: `${todos.length} items · ${open} open` };
    }
    case TOOL_NAMES.askUser:
      return { title, subject: '' };
    default:
      return { title, subject: fallbackArgsSummary(args) };
  }
}

// ── diff ──────────────────────────────────────────────────────────────

/** One row of a computed diff (before windowing / numbering). */
export interface DiffRow {
  readonly kind: 'context' | 'add' | 'del';
  readonly text: string;
}

/**
 * Myers line diff — minimal edit script, deletions before insertions inside a
 * change group (the conventional unified-diff order).
 *
 * `similar::TextDiff::from_lines` (the TUI's diff engine) produces the same
 * shape; determinism matters more than the exact script among equals because
 * the payload is already windowed to a handful of lines.
 */
export function lineDiff(oldLines: readonly string[], newLines: readonly string[]): DiffRow[] {
  // Trim the common prefix/suffix first: it keeps the expensive middle small.
  let start = 0;
  while (start < oldLines.length && start < newLines.length && oldLines[start] === newLines[start]) {
    start += 1;
  }
  let endOld = oldLines.length;
  let endNew = newLines.length;
  while (endOld > start && endNew > start && oldLines[endOld - 1] === newLines[endNew - 1]) {
    endOld -= 1;
    endNew -= 1;
  }

  const rows: DiffRow[] = [];
  for (let index = 0; index < start; index += 1) {
    rows.push({ kind: 'context', text: oldLines[index] ?? '' });
  }

  const midOld = oldLines.slice(start, endOld);
  const midNew = newLines.slice(start, endNew);
  rows.push(...myersDiff(midOld, midNew));

  for (let index = endOld; index < oldLines.length; index += 1) {
    rows.push({ kind: 'context', text: oldLines[index] ?? '' });
  }
  return rows;
}

/** Core Myers O(ND) shortest edit script over the trimmed middle section. */
function myersDiff(oldLines: readonly string[], newLines: readonly string[]): DiffRow[] {
  const n = oldLines.length;
  const m = newLines.length;
  if (n === 0 && m === 0) {
    return [];
  }
  if (n === 0) {
    return newLines.map((text): DiffRow => ({ kind: 'add', text }));
  }
  if (m === 0) {
    return oldLines.map((text): DiffRow => ({ kind: 'del', text }));
  }

  const max = n + m;
  const offset = max;
  const v = new Int32Array(2 * max + 2);
  const trace: Int32Array[] = [];
  let found = -1;

  outer: for (let d = 0; d <= max; d += 1) {
    trace.push(v.slice());
    for (let k = -d; k <= d; k += 2) {
      let x: number;
      if (k === -d || (k !== d && v[offset + k - 1]! < v[offset + k + 1]!)) {
        x = v[offset + k + 1]!; // insertion (move down)
      } else {
        x = v[offset + k - 1]! + 1; // deletion (move right)
      }
      let y = x - k;
      while (x < n && y < m && oldLines[x] === newLines[y]) {
        x += 1;
        y += 1;
      }
      v[offset + k] = x;
      if (x >= n && y >= m) {
        found = d;
        break outer;
      }
    }
  }

  // Backtrack: walk the trace backwards and collect the script in reverse.
  const reversed: DiffRow[] = [];
  let x = n;
  let y = m;
  for (let d = found; d > 0; d -= 1) {
    const previous = trace[d] ?? new Int32Array(2 * max + 2);
    const k = x - y;
    const down = k === -d || (k !== d && previous[offset + k - 1]! < previous[offset + k + 1]!);
    const previousK = down ? k + 1 : k - 1;
    const previousX = previous[offset + previousK]!;
    const previousY = previousX - previousK;
    while (x > previousX && y > previousY) {
      reversed.push({ kind: 'context', text: oldLines[x - 1] ?? '' });
      x -= 1;
      y -= 1;
    }
    if (down) {
      reversed.push({ kind: 'add', text: newLines[y - 1] ?? '' });
      y -= 1;
    } else {
      reversed.push({ kind: 'del', text: oldLines[x - 1] ?? '' });
      x -= 1;
    }
  }
  while (x > 0 && y > 0) {
    reversed.push({ kind: 'context', text: oldLines[x - 1] ?? '' });
    x -= 1;
    y -= 1;
  }
  reversed.reverse();
  return groupChanges(reversed);
}

/** Move every run of changes into "deletions first, then insertions" order. */
function groupChanges(rows: readonly DiffRow[]): DiffRow[] {
  const grouped: DiffRow[] = [];
  let index = 0;
  while (index < rows.length) {
    const row = rows[index];
    if (row === undefined) {
      break;
    }
    if (row.kind === 'context') {
      grouped.push(row);
      index += 1;
      continue;
    }
    const deletions: DiffRow[] = [];
    const insertions: DiffRow[] = [];
    while (index < rows.length) {
      const change = rows[index];
      if (change === undefined || change.kind === 'context') {
        break;
      }
      if (change.kind === 'del') {
        deletions.push(change);
      } else {
        insertions.push(change);
      }
      index += 1;
    }
    grouped.push(...deletions, ...insertions);
  }
  return grouped;
}

/** Result of turning a `diff_content` payload into a renderable window. */
export interface DiffWindowModel {
  readonly lines: readonly DiffLineModel[];
  readonly added: number;
  readonly removed: number;
  readonly truncated: boolean;
}

/**
 * Build the diff cell's rows for one payload window.
 *
 * The backend already windows `old_text` / `new_text` around the change (±3
 * context lines) and hands over the absolute start lines; we render exactly
 * that window (one `@@` header + numbered rows) and only apply a *transport*
 * cap — a `Write` of a large file carries the whole content, and one cell must
 * not turn into a megabyte of bridge traffic (see `truncated`).
 */
export function buildDiffWindow(input: {
  readonly oldText: string | null;
  readonly newText: string;
  readonly oldStartLine: number;
  readonly newStartLine: number;
  readonly maxRows: number;
}): DiffWindowModel {
  const oldLines = input.oldText === null ? [] : splitLines(input.oldText);
  const newLines = splitLines(input.newText);
  const rows = lineDiff(oldLines, newLines);
  const kept = rows.length > input.maxRows ? rows.slice(0, input.maxRows) : rows;
  const truncated = kept.length < rows.length;

  // 0 means "the payload carried no window information" → the window starts at 1.
  const oldStartLine = input.oldStartLine > 0 ? input.oldStartLine : 1;
  const newStartLine = input.newStartLine > 0 ? input.newStartLine : 1;
  let oldCursor = oldStartLine - 1;
  let newCursor = newStartLine - 1;
  const numbered: DiffLineModel[] = [];
  let added = 0;
  let removed = 0;
  for (const row of kept) {
    if (row.kind === 'del') {
      oldCursor += 1;
      removed += 1;
      numbered.push({ kind: 'del', text: row.text, oldLine: oldCursor, newLine: null });
    } else if (row.kind === 'add') {
      newCursor += 1;
      added += 1;
      numbered.push({ kind: 'add', text: row.text, oldLine: null, newLine: newCursor });
    } else {
      oldCursor += 1;
      newCursor += 1;
      numbered.push({ kind: 'context', text: row.text, oldLine: oldCursor, newLine: newCursor });
    }
  }

  const oldCount = numbered.filter((row) => row.oldLine !== null).length;
  const newCount = numbered.filter((row) => row.newLine !== null).length;
  // Same formula as the TUI's `flush_block`: a side that contributes no line
  // shows the preceding line number (git's convention).
  const oldStart = oldCount === 0 ? oldStartLine - 1 : oldStartLine;
  const newStart = newCount === 0 ? newStartLine - 1 : newStartLine;
  const hunk: DiffLineModel = {
    kind: 'hunk',
    text: `@@ -${oldStart},${oldCount} +${newStart},${newCount} @@`,
    oldLine: null,
    newLine: null,
  };
  return { lines: [hunk, ...numbered], added, removed, truncated };
}

/** Host-side transport cap for one diff window (rows, excluding the header). */
export const DIFF_MAX_ROWS = 800;

// ── ask ───────────────────────────────────────────────────────────────

/** Normalized ask payload: the two wire shapes collapse into one model. */
export interface NormalizedAsk {
  readonly questions: readonly AskQuestionModel[];
  /** True for the retired Bash confirmation (single required choice; bare-label reply). */
  readonly approval: boolean;
  /**
   * `false` for the retired *non-required* shape: nothing is answerable, so the
   * cell must not offer an answer (replying would fall through into a stray
   * user message on the backend).
   */
  readonly answerable: boolean;
}

/** Legacy single-question id (TUI `LEGACY_QUESTION_ID`). */
export const LEGACY_ASK_QUESTION_ID = 'choice';

function normalizeQuestion(question: {
  readonly id: string;
  readonly header: string;
  readonly question: string;
  readonly multiSelect: boolean;
  readonly options: readonly { readonly label: string; readonly description: string }[];
  readonly choices: readonly string[];
}): AskQuestionModel {
  const options =
    question.options.length > 0
      ? question.options
      : question.choices.map((label) => ({ label, description: '' }));
  return {
    id: question.id,
    question: question.question,
    header: question.header,
    multiSelect: question.multiSelect,
    options,
    // AskUserQuestion questions may be skipped (the backend turns a missing
    // answer into `(user did not answer)`), so they never gate submit.
    required: false,
  };
}

/**
 * Normalize either wire shape into the shared ask model (TUI `from_ask`).
 *
 * `questions` wins when both shapes are present; the retired
 * `question`/`choices`/`required` form becomes a single required question whose
 * reply is the bare option label (`y` / `n` / `yolo` are the only values the
 * backend's `_parse_feedback` accepts).
 */
export function normalizeAsk(event: AskEvent): NormalizedAsk {
  if (event.questions.length > 0) {
    return {
      questions: event.questions.map((question) => normalizeQuestion(question)),
      approval: false,
      answerable: true,
    };
  }
  const options = event.choices.map((label) => ({ label, description: '' }));
  const question: AskQuestionModel = {
    id: LEGACY_ASK_QUESTION_ID,
    question: event.question,
    header: '',
    multiSelect: false,
    options,
    required: true,
  };
  const approval = event.required && options.length > 0;
  return { questions: [question], approval, answerable: approval };
}

/** Tab label of a question: header when present, else the id. */
export function askTabLabel(question: AskQuestionModel): string {
  return question.header.trim() === '' ? question.id : question.header;
}

/** One question's answer value, `null` when the user left it unanswered. */
export function askAnswerValue(
  question: AskQuestionModel,
  answer: AskAnswerModel | undefined,
): string | null {
  if (answer === undefined) {
    return null;
  }
  if (question.multiSelect) {
    // Option order (not click order) — the TUI reads the toggles in option order.
    const parts = question.options
      .map((option) => option.label)
      .filter((label) => answer.selected.includes(label));
    const custom = answer.text.trim();
    if (custom !== '') {
      parts.push(custom);
    }
    return parts.length === 0 ? null : parts.join(', ');
  }
  if (answer.selected.length > 0) {
    return answer.selected[0] ?? null;
  }
  const custom = answer.text.trim();
  return custom === '' ? null : custom;
}

/**
 * Build the wire reply for one ask (TUI `AskPanel::build_response`).
 *
 * - approval / required-choice: the bare option label (backend `_parse_feedback`);
 * - question form: one `header: answer` line per question, `(user did not answer)`
 *   for skipped ones.
 */
export function buildAskReply(
  input: {
    readonly approval: boolean;
    readonly questions: readonly AskQuestionModel[];
  },
  answers: readonly AskAnswerModel[],
): string {
  if (input.approval) {
    const first = answers[0];
    return first?.selected[0] ?? '';
  }
  return input.questions
    .map((question) => {
      const answer = answers.find((candidate) => candidate.questionId === question.id);
      const value = askAnswerValue(question, answer);
      return `${askTabLabel(question)}: ${value ?? ASK_UNANSWERED}`;
    })
    .join('\n');
}

// ── branch targets ────────────────────────────────────────────────────

/** Map a `branch_targets` payload entry into a panel row. */
export function branchRow(target: BranchTarget): {
  uuid: string;
  role: string;
  preview: string;
  current: boolean;
} {
  const current = target.uuid === 'current';
  return {
    uuid: target.uuid,
    role: target.role,
    preview: truncateChars(target.content, 100),
    current,
  };
}

// ── todos ─────────────────────────────────────────────────────────────

/**
 * `TodoWrite` items out of the tool arguments (TUI `TodoMessage::from_tool_args`).
 *
 * `null` when the payload has no `todos` array at all — the caller then leaves
 * the tool call without a todo cell. An unknown status collapses to `pending`
 * (the wire carries free-form strings; the shared model has three values).
 */
export function todoItemsFromArgs(args: JsonValue | null): TodoItemModel[] | null {
  const object = asObject(args);
  const todos = asArray(object?.['todos']);
  if (todos === null) {
    return null;
  }
  return todos.map((item) => {
    const record = asObject(item);
    const content = record === null ? '' : stringField(record, 'content');
    const rawStatus = record === null ? '' : stringField(record, 'status');
    const status: TodoItemStatus =
      rawStatus === 'in_progress' || rawStatus === 'completed' ? rawStatus : 'pending';
    return { content, status };
  });
}
