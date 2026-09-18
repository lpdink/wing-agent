import type { CellModel, HostToWebviewMessage } from '../../../src/shared';
import { applyCellPatches, isExpectedSeq } from '../../../src/webview/state/applyPatch';

/**
 * The webview's mirror, driven by the host's real messages.
 *
 * It intentionally reuses the *shipped* consumer reducer
 * (`src/webview/state/applyPatch.ts`) instead of reimplementing patch
 * application: when a host test asserts "the webview sees X", it asserts that
 * the frozen implementation can follow the host's patch stream. A host bug that
 * violates `seq` continuity or addresses an unknown cell shows up here as a
 * `resync`-worthy failure instead of a silently different transcript.
 */
export class WebviewMirror {
  private readonly sessions = new Map<string, { seq: number; cells: readonly CellModel[] }>();
  private readonly failures: string[] = [];

  apply(message: HostToWebviewMessage): void {
    switch (message.type) {
      case 'hydrate':
        this.sessions.set(message.session.sessionId, {
          seq: message.session.seq,
          cells: message.session.cells,
        });
        return;
      case 'patch': {
        const session = this.sessions.get(message.sessionId);
        if (session === undefined) {
          this.failures.push(`patch for unknown session ${message.sessionId}`);
          return;
        }
        if (!isExpectedSeq(session.seq, message.seq)) {
          this.failures.push(
            `seq gap for ${message.sessionId}: got ${message.seq}, expected ${session.seq + 1}`,
          );
          return;
        }
        const result = applyCellPatches(session.cells, message.patches);
        if (!result.ok) {
          this.failures.push(`patch rejected for ${message.sessionId}: ${result.reason}`);
          return;
        }
        this.sessions.set(message.sessionId, { seq: message.seq, cells: result.cells });
        return;
      }
      default:
        // `state` / `panels` / `tabs` / `ui` / `pong` do not touch the transcript.
        return;
    }
  }

  cells(sessionId: string): readonly CellModel[] {
    return this.sessions.get(sessionId)?.cells ?? [];
  }

  seq(sessionId: string): number {
    return this.sessions.get(sessionId)?.seq ?? -1;
  }

  get errors(): readonly string[] {
    return this.failures;
  }
}

/** Feed a whole capture (in order) through the mirror. */
export function mirror(posted: readonly HostToWebviewMessage[]): WebviewMirror {
  const view = new WebviewMirror();
  for (const message of posted) {
    view.apply(message);
  }
  return view;
}

/** Kinds of a cell list, for compact ordering assertions. */
export function kinds(cells: readonly CellModel[]): string[] {
  return cells.map((cell) => cell.kind);
}
