# `src/core` — gateway capability layer (step 02)

Layer contract (enforced by lint + `tests/layers.test.ts`):

- Talks to the wing gateway: protocol mirror, WS/HTTP clients, chunk reassembly, reconnect.
- **No `vscode`**, no DOM, no UI concepts — it only needs `fetch` / `WebSocket` (Node 22 has both),
  so the same code can be lifted into an Electron or web frontend later.
- May import `src/shared` (contract types) and `src/core` only.

Step 01 ships the directory (and this layer declaration) so the dependency matrix has a
target; step 02 fills it with the client.
