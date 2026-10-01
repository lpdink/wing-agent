# Third-party notices

This product includes software developed by third parties. The notices below
cover every third-party source that has been copied into this repository (as
opposed to consumed as a dependency, which the lockfiles track).

---

## DeepSeek Harness (MIT)

Portions of this software are derived from DeepSeek Harness
(https://github.com/deepseek-ai/deepseek-harness), Copyright (c) 2026 DeepSeek,
and are used under the MIT License.

All derived files live in `packages/ui` (steps 06b and 06c of the wing-app work:
theme tokens, code cards, terminal output, diffs, the process rows, the ask trio
and the shared atoms). Each derived file carries a header naming its source path
and the modifications made for Wing.

### Derived source files

Theme token sheets — `packages/ui/src/styles/` (from
`packages/client/ui-theme/src/styles/`):

- `design-platform.css` — the light/dark token table. Modified: the brand family
  is renamed `--dsw-static-wing-*` and carries Wing's own blue ramp (the welcome
  gull's navy `#1B2338` / slate `#3A4763` plus a sky-blue ladder); the light
  `--dsw-alias-brand-primary-new-colorprimary-new-color` literal now points at
  that family. Port batch 06c appends the elevation trio
  (`--dsw-shadow-lv2`, `--dsw-elevation-stroke`, `--dsw-elevation-panel`) from
  `ui-theme/src/styles/gradient-shadow-text.css:5-34`, which the port does not
  carry as a sheet and the ported approval/ask cards read.
- `base.css` — modified: the two font aliases the ported components read
  (`--dsw-font-markdown-code-block`, `--dsw-font-xs-13`) are supplied here, since
  the upstream sheet that defines them (`gradient-shadow-text.css`) is not part
  of this port. Port batch 06c adds the content-size axis
  (`--dsh-content-font-delta`, `--dsh-content-font-size-secondary`,
  `--dsh-content-font-delta-secondary`, from the same sheet) and the seat widths
  the ported ask card reads (`--dsh-chat-content-width`,
  `--dsh-composer-side-clearance`, `--dsh-composer-text-max-height`, from
  `ui-conversation/.../ConversationRoot.module.css`).
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
  the language hint comes from a local extension table (exported so a test can pin
  every entry to a grammar the highlighter actually ships). The `path` row class is
  dropped (the path rides the toolbar's title slot) and `added` / `removed` /
  `hunk` classes are added; two data hooks the tests read are Wing additions
  (`data-testid="diff-body"` on the body, `data-diff-kind` on every row).

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
  `tests/markdown/code-block.test.tsx`, `tests/markdown/use-viewport-highlighting.test.tsx`
  — derived from `packages/client/ui-primitives/tests/`
  (`ansi.client.spec.ts`, `terminal-block.client.spec.tsx`,
  `diff-block.client.spec.tsx`, `code-block.client.spec.tsx`,
  `code-card-controls.client.spec.tsx`, `highlight-viewport.client.spec.tsx`).
  Modified: the label fixtures are this package's own English constants (the
  components take copy via props rather than a locale), the diff fixtures are
  `DiffCellModel`s rather than before/after text, the streaming-highlight cases are
  not ported (the streaming session itself is not), and the viewport spec drives
  this package's `CodeBlock` through a scripted `IntersectionObserver`.

### Derived source files — port batch 06c (process rows, ask trio, connection indicator)

Process rows — `packages/ui/src/chat/`:

- `DisclosureRow.tsx` (+ `DisclosureRow.module.css`) — from
  `packages/client/ui-primitives/src/`. Modified: the chevrons come from this
  package's `icons` module instead of the upstream primitives barrel; markup and
  behaviour are unchanged.
- `TextShimmer.tsx` (+ `TextShimmer.module.css`) — from
  `packages/client/ui-primitives/src/`. Modified: `inert` is written as the
  React 19 boolean attribute (the upstream `{...{ inert: '' }}` spread is the
  React 18 spelling of the same thing).
- `ReasoningRow.tsx` (+ `ReasoningRow.module.css`, `accessibility.module.css`)
  — from `packages/client/ui-chat/src/client/chat/`. Modified: the expanded body
  renders this package's `MarkdownStream` (the upstream `MarkdownText` belongs
  to a markdown pipeline this port does not ship); the host slot contract
  (`useDisclosure` / `usePresentation` / `t()`) is replaced by local or
  remembered disclosure state, a `previewEnabled` prop and label props.

Ask — `packages/ui/src/ask/`:

- `ApprovalPanel.tsx` (+ `ApprovalPanel.module.css`) — from
  `packages/client/ui-approval/src/client/`. Modified: the slot/carrier contract
  (`renderSlot`, `matched`, `pending.answer`, the promise rollback) is replaced
  by plain props (`detail` is a `ReactNode`, decisions go through `onDecide`),
  and the copy is a label prop.
- `QuestionComposer.tsx` (+ `QuestionComposer.module.css`) — from
  `packages/client/ui-user-questions/src/client/`. Modified: the draft store,
  the countdown/wait channel, the plan-review takeover, minimize/close and the
  markdown `detail` are not ported; the per-question cards, the progress pager,
  single/multi select, the `(recommended)` suffix, the free-form text with IME
  protection and the submit-time completeness check are, over this package's
  `AskQuestionModel`.
- `QuestionReplyView.tsx` (+ `QuestionReplyView.module.css`) — from
  `packages/client/ui-user-questions/src/client/`. Modified: the slot contract
  and the locale seat are replaced by props over `AskQuestionModel` /
  `AskAnswerModel`; the copy control writes the clipboard through this
  package's clipboard helper; the details hairline is a static token instead of
  a `color-mix()` (the package's colour gate rejects mix functions; the visual
  delta is reviewed in the preview screenshots).
- `question-reply.ts` — from
  `packages/client/ui-user-questions/src/client/question-reply.ts`. Modified:
  trimmed to the two pure projections over this package's answer model
  (`replyAnswerValues`, `replyClipboardText`); the upstream session projection
  belongs to a runtime this repository does not have, and `t()` is replaced by
  a labels argument.

Atoms — `packages/ui/src/components/`:

- `Button.tsx` (+ `Button.module.css`) — from
  `packages/client/ui-primitives/src/`. Modified: unchanged (the `forwardRef`
  wrapper is kept verbatim; React 19 accepts it).
- `ConnectionIndicator.tsx` (+ `ConnectionIndicator.module.css`) — from
  `packages/client/ui-primitives/src/`. Modified: the icons come from this
  package's `icons` module and the three `color-mix()` hairlines are rewritten
  onto the nearest static tokens (same colour gate as above); the state/label/
  reconnect props are unchanged.
- `icons/index.tsx` (extended) — the same extraction as batch 06b, plus the
  eight glyphs batch 06c draws (`IconChevronDown/Up/Left/Right`,
  `IconCloseOutlineRegular`, `IconRefreshOutlineRegular`,
  `IconEditOutlineRegular`, `IconThinkOutlineRegular`).

Ask/row/atom tests — `packages/ui/tests/`:

- `tests/chat/rows.test.tsx` — from
  `packages/client/ui-primitives/tests/text-shimmer.client.spec.tsx` and
  `disclosure-row-styles.client.spec.ts`.
- `tests/chat/reasoning-row.test.tsx` — from
  `packages/client/ui-chat/tests/reasoning-row.client.spec.tsx`.
- `tests/ask/approval-panel.test.tsx` — from `packages/client/ui-approval/tests/`.
- `tests/ask/question-composer.test.tsx` — from
  `packages/client/ui-user-questions/tests/question-composer.client.spec.tsx`.
- `tests/ask/question-reply.test.tsx`, `tests/ask/question-reply.test.ts` —
  from `packages/client/ui-user-questions/src/client/QuestionReplyView.tsx` and
  `question-reply.ts` (the trimmed projections' contract).
- `tests/components/button.test.tsx` — from
  `packages/client/ui-primitives/tests/atoms.client.spec.tsx` (button cases).
- `tests/components/connection-indicator.test.tsx` — from
  `packages/client/ui-primitives/src/ConnectionIndicator.tsx` (the component's
  own contract: three states, the reconnect sink, the exit transition).
  Modified (all of the above): rendered through `@wing-agent/ui`'s barrel, the
  shell-side state mapping and slot contracts are replaced by the props this
  package ships, and the locale fixtures are English label constants.

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
