import path from 'node:path';

import { describe, expect, it } from 'vitest';

import { resolveWorkspaceImage, resolveWorkspacePath } from '../../src/host/images';

/**
 * The image path policy.
 *
 * Everything here is a pure function of `(workspace root, markdown src)`, which is
 * the point: the renderer never touches the disk, and "which sources may be shown"
 * is decided (and tested) in one place. `null` means "keep the link the transcript
 * always rendered".
 */

const ROOT = path.resolve('/workspace/project');

/** Inside the workspace, in the platform's own spelling. */
function inside(...segments: readonly string[]): string {
  return path.join(ROOT, ...segments);
}

describe('resolveWorkspaceImage (accepted)', () => {
  it('resolves a path relative to the workspace', () => {
    expect(resolveWorkspaceImage(ROOT, 'plot.png')).toBe(inside('plot.png'));
    expect(resolveWorkspaceImage(ROOT, 'docs/img/plot.png')).toBe(inside('docs/img/plot.png'));
    expect(resolveWorkspaceImage(ROOT, './docs/../plot.png')).toBe(inside('plot.png'));
  });

  it('accepts an absolute path inside the workspace', () => {
    expect(resolveWorkspaceImage(ROOT, inside('build', 'plot.png'))).toBe(inside('build', 'plot.png'));
  });

  it('decodes the percent-escapes markdown-it writes', () => {
    // `![a](<my image.png>)` reaches the webview as `my%20image.png`.
    expect(resolveWorkspaceImage(ROOT, 'my%20image.png')).toBe(inside('my image.png'));
    // …and a Windows-looking absolute path comes back as `C:%5C…`.
    expect(resolveWorkspaceImage(ROOT, 'docs/a%2Bb.png')).toBe(inside('docs/a+b.png'));
  });

  it('accepts every extension a webview can render, case-insensitively', () => {
    for (const extension of ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'ico', 'avif', 'apng']) {
      expect({ extension, resolved: resolveWorkspaceImage(ROOT, `plot.${extension}`) }).toEqual({
        extension,
        resolved: inside(`plot.${extension}`),
      });
      expect(resolveWorkspaceImage(ROOT, `PLOT.${extension.toUpperCase()}`)).toBe(
        inside(`PLOT.${extension.toUpperCase()}`),
      );
    }
  });

  it('tolerates surrounding whitespace in the source', () => {
    expect(resolveWorkspaceImage(ROOT, '  plot.png  ')).toBe(inside('plot.png'));
  });
});

describe('resolveWorkspaceImage (refused → the renderer keeps its link)', () => {
  const refused: readonly (readonly [name: string, src: string])[] = [
    ['empty source', ''],
    ['whitespace only', '   '],
    ['http url', 'https://example.com/plot.png'],
    ['protocol-relative url', '//example.com/plot.png'],
    ['data url', 'data:image/png;base64,iVBORw0KGgo='],
    ['file url', 'file:///etc/plot.png'],
    ['vscode resource url', 'vscode-resource://x/plot.png'],
    ['query suffix', 'plot.png?raw=1'],
    ['fragment suffix', 'plot.png#anchor'],
    ['non-image extension', 'notes.md'],
    ['extensionless file', 'Makefile'],
    ['no extension at all', 'plot'],
    ['escape via ..', '../outside.png'],
    ['escape via nested ..', 'docs/../../outside.png'],
    ['absolute outside the workspace', path.resolve('/etc/shadow.png')],
    ['sibling prefix', `${ROOT}-other/plot.png`],
    ['the workspace folder itself', '.'],
    ['control character', 'plot\u0000.png'],
    ['newline', 'plot\n.png'],
    ['over-long source', `${'a'.repeat(1024)}.png`],
  ];

  it.each(refused)('refuses %s', (_name, src) => {
    expect(resolveWorkspaceImage(ROOT, src)).toBeNull();
  });

  it('refuses everything that is not absolute when no folder is open', () => {
    expect(resolveWorkspaceImage(null, 'plot.png')).toBeNull();
    expect(resolveWorkspaceImage(null, '../plot.png')).toBeNull();
    // Even an absolute path: without a root there is no way to tell whether it is
    // inside the workspace, and nothing outside it may be loaded.
    expect(resolveWorkspaceImage(null, path.resolve('/tmp/plot.png'))).toBeNull();
  });

  it('cannot be tricked by an encoded escape', () => {
    // Decoding happens *before* the containment check, so an encoded `../` is still
    // an escape (and `%2F` cannot reintroduce a root the caller did not grant).
    expect(resolveWorkspaceImage(ROOT, '%2e%2e%2Foutside.png')).toBeNull();
    expect(resolveWorkspaceImage(ROOT, 'docs%2F..%2F..%2Foutside.png')).toBeNull();
  });

  it('never throws, whatever the source looks like', () => {
    const sources = [
      '%',
      '%zz.png',
      '\\\\server\\share\\plot.png',
      './',
      '..',
      '.png',
      '...png',
      '\u0001.png',
    ];
    for (const src of sources) {
      expect(() => resolveWorkspaceImage(ROOT, src)).not.toThrow();
    }
  });
});

describe('resolveWorkspacePath', () => {
  it('joins a relative path onto the workspace root (the `openFile` rule)', () => {
    expect(resolveWorkspacePath(ROOT, 'src/main.ts')).toBe(inside('src/main.ts'));
  });

  it('passes an absolute path through unchanged', () => {
    expect(resolveWorkspacePath(ROOT, '/etc/hosts')).toBe('/etc/hosts');
  });

  it('returns the candidate verbatim when there is no workspace', () => {
    expect(resolveWorkspacePath(null, 'src/main.ts')).toBe('src/main.ts');
  });

  it('does not police the workspace (that is `resolveWorkspaceImage`’s job)', () => {
    // Documented difference: opening a file the model mentioned outside the
    // workspace is legitimate; *loading* it into the webview is not.
    expect(resolveWorkspacePath(ROOT, '../elsewhere/file.ts')).toBe(
      path.resolve('/workspace/elsewhere/file.ts'),
    );
    expect(resolveWorkspaceImage(ROOT, '../elsewhere/file.png')).toBeNull();
  });
});
