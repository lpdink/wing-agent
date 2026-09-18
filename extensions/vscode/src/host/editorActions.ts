import * as vscode from 'vscode';

import type { DiffCellModel } from '../shared';

import { log } from './log';

/**
 * Editor-side actions the session layer asks for.
 *
 * Injected (not imported directly) so host tests can record intent without an
 * editor: `SessionManager` must never call `vscode` itself beyond this seam.
 */
export interface EditorActions {
  /** Open an http(s) link in the OS browser (anything else is refused). */
  openLink(href: string): Promise<void>;
  /** Open a file, optionally at a line, in an editor tab. */
  openFile(path: string, line: number | null): Promise<void>;
  /**
   * Show a diff for a `diff` cell.
   *
   * Webviews cannot build a diff editor themselves, and the payload is a
   * *window* (changed region ± context) — so the host serves the reconstructed
   * window texts through a virtual document scheme and lets VS Code render the
   * native diff.
   */
  openDiff(cell: DiffCellModel): Promise<void>;
  /** Copy text through the editor clipboard. */
  copyText(text: string): Promise<void>;
}

/** Scheme for the virtual documents backing {@link openDiff}. */
export const DIFF_SCHEME = 'wing-diff';

/** Reconstruct the window's old/new text from the numbered diff rows. */
export function diffWindowTexts(cell: DiffCellModel): { oldText: string; newText: string } {
  const oldLines: string[] = [];
  const newLines: string[] = [];
  for (const line of cell.lines) {
    if (line.kind === 'hunk') {
      continue;
    }
    if (line.oldLine !== null) {
      oldLines.push(line.text);
    }
    if (line.newLine !== null) {
      newLines.push(line.text);
    }
  }
  return { oldText: oldLines.join('\n'), newText: newLines.join('\n') };
}

/**
 * VS Code implementation of {@link EditorActions}.
 *
 * `openDiff` registers a content provider for `wing-diff:` URIs and keeps the
 * window texts in memory until the editor closes them. Documents are keyed by
 * the cell id, so re-opening the same cell reuses the same content.
 */
export class VsCodeEditorActions implements EditorActions, vscode.Disposable {
  private readonly documents = new Map<string, string>();
  private readonly registration: vscode.Disposable;
  private readonly resolvePath: (path: string) => string;

  /**
   * @param options.resolvePath Turns a possibly-relative path (a tool call's
   *   subject, e.g. `src/a.ts`) into an absolute one. Defaults to identity: the
   *   extension passes a workspace-folder resolver.
   */
  constructor(options: { readonly resolvePath?: (path: string) => string } = {}) {
    this.resolvePath = options.resolvePath ?? ((path) => path);
    this.registration = vscode.workspace.registerTextDocumentContentProvider(DIFF_SCHEME, {
      provideTextDocumentContent: (uri: vscode.Uri): string => {
        return this.documents.get(uri.toString()) ?? '';
      },
    });
  }

  async openLink(href: string): Promise<void> {
    let uri: vscode.Uri;
    try {
      uri = vscode.Uri.parse(href, true);
    } catch {
      log().warn(`[actions] refusing malformed link: ${href}`);
      return;
    }
    if (uri.scheme !== 'http' && uri.scheme !== 'https') {
      log().warn(`[actions] refusing non-http link: ${href}`);
      return;
    }
    await vscode.env.openExternal(uri);
  }

  async openFile(path: string, line: number | null): Promise<void> {
    const uri = vscode.Uri.file(this.resolvePath(path));
    const options: vscode.TextDocumentShowOptions = { preview: true };
    if (line !== null && line > 0) {
      const position = new vscode.Position(Math.max(0, line - 1), 0);
      options.selection = new vscode.Range(position, position);
    }
    await vscode.window.showTextDocument(uri, options);
  }

  async openDiff(cell: DiffCellModel): Promise<void> {
    const { oldText, newText } = diffWindowTexts(cell);
    const oldUri = this.documentUri(`${cell.id}-old`, cell.path, cell.oldStartLine);
    const newUri = this.documentUri(`${cell.id}-new`, cell.path, cell.newStartLine);
    this.documents.set(oldUri.toString(), oldText);
    this.documents.set(newUri.toString(), newText);
    await vscode.commands.executeCommand(
      'vscode.diff',
      oldUri,
      newUri,
      `${cell.path} (${oldText === '' ? 'old → new' : 'wing diff'})`,
      { preview: true },
    );
  }

  async copyText(text: string): Promise<void> {
    await vscode.env.clipboard.writeText(text);
  }

  dispose(): void {
    this.documents.clear();
    this.registration.dispose();
  }

  private documentUri(key: string, path: string, startLine: number): vscode.Uri {
    const name = path.split(/[/\\]/).pop() ?? path;
    return vscode.Uri.parse(`${DIFF_SCHEME}:${key}/${name}?line=${startLine}`, true);
  }
}
