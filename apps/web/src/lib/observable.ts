/**
 * The smallest observable a React shell needs.
 *
 * `GatewayRuntime` mutates its own plain-object state and then calls
 * `notifier.notify()`; React reads it through `useSyncExternalStore(subscribe,
 * getSnapshot)`. The notifier itself only owns the listener set — the snapshot
 * caching (the part `useSyncExternalStore` insists on: `getSnapshot` must return
 * the *same* reference until something changed) lives in the runtime.
 *
 * Listener exceptions are contained (one broken subscriber must not stop the
 * others) but never swallowed silently: they go to `console.error`, which the
 * lint config allows for exactly this reason.
 */
export class Notifier {
  private readonly listeners = new Set<() => void>();

  /** Subscribe; returns the unsubscribe function (idempotent). */
  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  get listenerCount(): number {
    return this.listeners.size;
  }

  notify(): void {
    for (const listener of [...this.listeners]) {
      try {
        listener();
      } catch (error) {
        console.error('observable listener threw', error);
      }
    }
  }
}
