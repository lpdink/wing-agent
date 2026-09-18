/**
 * `src/core/protocol` — the typed mirror of the gateway wire protocol.
 *
 * Modules:
 * - `json.ts`    — payload reading primitives (`req*` / `opt*` / `read*`) + `JsonObject`
 * - `models.ts`  — nested structures shared by events and HTTP (AgentInfo, Ask, BranchTarget)
 * - `events.ts`  — the `WingEvent` union, its decoders and the type registry
 * - `history.ts` — the Message projection (`SessionMessage`) and uncommitted tool calls
 * - `frames.ts`  — WS handshake + client → server frames
 * - `http.ts`    — every HTTP endpoint's request / response model + decoders
 *
 * Python is the source of truth; each module names the file it mirrors.
 */

export * from './events';
export * from './frames';
export * from './history';
export * from './http';
export * from './json';
export * from './models';
