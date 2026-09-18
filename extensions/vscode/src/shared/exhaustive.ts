/**
 * Exhaustiveness helpers for discriminated unions.
 *
 * Two flavours, because the right failure mode depends on where the union is
 * consumed:
 *
 * - {@link assertNever} (in `./cells`) **throws** — correct for pure logic
 *   (reducers, renderers, fixtures) where an unhandled variant is a programming
 *   error that must surface immediately in tests.
 * - {@link unhandledVariant} **warns** — correct for a live message channel or a
 *   long-running state machine: an unknown variant must never take the channel
 *   down, but it must not disappear silently either.
 *
 * Both are compile-time gates first: they only accept `never`, so adding a variant
 * without handling it becomes a type error at every call site.
 */

/**
 * Report a variant that should be unreachable.
 *
 * Returns `void` so it can be used as `default: unhandledVariant(x, '…')` in a
 * switch over a union that is fully handled.
 */
export function unhandledVariant(value: never, context: string): void {
  console.warn(`[wing] ${context}: unhandled variant`, value);
}
