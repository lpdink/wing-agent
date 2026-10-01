# Third-party notices

This product includes software developed by third parties. The notices below
cover every third-party source that has been copied into this repository (as
opposed to consumed as a dependency, which the lockfiles track).

---

## DeepSeek Harness (MIT)

Portions of this software are derived from DeepSeek Harness
(https://github.com/deepseek-ai/deepseek-harness), Copyright (c) 2026 DeepSeek,
and are used under the MIT License.

All derived files live in `packages/ui` (step 06b of the wing-app work: theme
tokens, code cards, terminal output and diffs). Each derived file carries a
header naming its source path and the modifications made for Wing.

### Derived source files

Theme token sheets — `packages/ui/src/styles/` (from
`packages/client/ui-theme/src/styles/`):

- `design-platform.css` — the light/dark token table. Modified: the brand family
  is renamed `--dsw-static-wing-*` and carries Wing's own blue ramp (the welcome
  gull's navy `#1B2338` / slate `#3A4763` plus a sky-blue ladder); the light
  `--dsw-alias-brand-primary-new-colorprimary-new-color` literal now points at
  that family.
- `base.css` — modified: the two font aliases the ported components read
  (`--dsw-font-markdown-code-block`, `--dsw-font-xs-13`) are supplied here, since
  the upstream sheet that defines them (`gradient-shadow-text.css`) is not part
  of this port.
- `scrollbar.css`, `focus.css`, `shiki.css` — copied unchanged (the
  `data-input-modality` rule in `focus.css` is inert until an input-modality port
  lands, and is kept so the behaviour is not silently dropped when it does).

Code cards — `packages/ui/src/markdown/`:

- `CodeBlock.tsx` (+ `CodeBlock.module.css`) — from
  `packages/client/ui-primitives/src/markdown/`. Modified: the streaming
  highlight session and its per-line React cache are not ported (a growing fence
  renders plain and is highlighted once, when it settles); highlighting goes
  through this package's `highlightToHtml`; the copy control writes the browser
  clipboard through this package's `clipboard.ts`; the plain copy button's
  `background-color: rgb(255 255 255 / 0)` is written as `transparent` (the
  package's colour gate rejects literal colours; same computed value).
- `CodeToolbar.tsx` — from `packages/client/ui-primitives/src/`. Modified: the
  `Tooltip` dependency is replaced by the native `title` attribute; `status`
  widens from `string` to `ReactNode` so a card can pass coloured counters.
- `CodeCard.module.css` — from `packages/client/ui-primitives/src/`. Modified:
  the copy button's `background-color` literal is written as `transparent`.
- `useViewportHighlighting.ts` — from
  `packages/client/ui-primitives/src/markdown/`. Modified: `supportsHighlighting`
  comes from this package's highlight module.

Terminal output — `packages/ui/src/tool/`:

- `TerminalBlock.tsx` (+ `TerminalBlock.module.css`), `ansi.ts`,
  `head-tail-cap.ts`, `FoldToggle.tsx`, `use-copy-feedback.ts`, `clipboard.ts`
  — from `packages/client/ui-primitives/src/`. Modified: imports point at this
  package's modules (`Pill` / `StateDot` live under `src/components`), and the
  component's comments no longer reference a plugin runtime this repository does
  not have. Behaviour is otherwise unchanged.

Diffs — `packages/ui/src/tool/`:

- `DiffBlock.tsx` (+ `DiffBlock.module.css`) — from
  `packages/client/ui-primitives/src/`. Modified: the data source is Wing's
  `DiffCellModel` (the host already computed and windowed the rows), so the
  `diff` package (`structuredPatch`), the `DiffHunk` contract and `diffTotals`
  are gone; rows map one-to-one onto the card's line classes, a windowed payload
  appends the `…` row, the `+n/−m` counters ride the toolbar's status slot, and
  the language hint comes from a local extension table. The `path` row class is
  dropped (the path rides the toolbar's title slot) and `added` / `removed` /
  `hunk` classes are added.

Dependency pieces brought in early — `packages/ui/src/components/`,
`packages/ui/src/icons/`:

- `StateDot.tsx` (+ `.module.css`), `Pill.tsx` (+ `.module.css`) — from
  `packages/client/ui-primitives/src/`. Modified: imports point at this package's
  modules; no behaviour change.
- `icons/index.tsx` — extracted from
  `packages/client/ui-primitives/src/icons/index.tsx` (+ `icons/props.ts`):
  only the four glyphs the code cards use (`IconCopyOutlineRegular`,
  `IconCheckOutlineRegular`, `IconWrapFillRegular`, `IconNowrapFillRegular`) and
  the `IconProps` shape; the shared weighted-artwork indirection is inlined.

Tests (kept in-repo as the port's regression suite) — `packages/ui/tests/`:

- `tests/tool/ansi.test.ts`, `tests/tool/terminal-block.test.tsx`,
  `tests/tool/diff-block.test.tsx`, `tests/tool/head-tail-cap.test.ts`,
  `tests/markdown/code-block.test.tsx` — derived from
  `packages/client/ui-primitives/tests/` (`ansi.client.spec.ts`,
  `terminal-block.client.spec.tsx`, `diff-block.client.spec.tsx`,
  `code-block.client.spec.tsx`, `code-card-controls.client.spec.tsx`).
  Modified: the label fixtures are this package's own English constants (the
  components take copy via props rather than a locale), the diff fixtures are
  `DiffCellModel`s rather than before/after text, and the streaming-highlight
  cases are not ported (the streaming session itself is not).

### MIT License

MIT License

Copyright (c) 2026 DeepSeek

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
