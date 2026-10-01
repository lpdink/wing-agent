// A consumer shaped like `apps/web`: it imports a transcript component from the
// package barrel and never touches `mountApp`.
//
// This file exists for `tests/artifacts/consumer-build.test.ts`, which builds it
// through the repository's own Vite and asserts properties of the *emitted* CSS —
// properties that are invisible to source-level gates. It deliberately mirrors how
// a real shell consumes the package: the bare specifier resolves through the
// package's `exports` map, exactly like `import { … } from '@wing-agent/ui'` in an
// application (Node's package self-reference).

import { CellView, TranscriptView } from '@wing-agent/ui';

export const consumerProbe = { CellView, TranscriptView };
