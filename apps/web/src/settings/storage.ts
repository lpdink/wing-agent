/**
 * Where the gateway connection settings live between visits.
 *
 * Two implementations: the browser's `localStorage` (the real one) and an
 * in-memory map (tests, and the fallback when storage is unavailable — Safari
 * private mode throws on `setItem`, a hardened browser may remove the object
 * entirely, an iframe with a `data:` URL has an opaque origin).
 *
 * `persistent` is part of the interface on purpose: when storage is unavailable
 * the settings dialog says so instead of pretending the value was saved.
 */

export interface SettingsStorage {
  read(key: string): string | null;
  write(key: string, value: string): void;
  remove(key: string): void;
  /** `false` = writes only live for this page load (memory fallback). */
  readonly persistent: boolean;
}

/** In-memory storage; seeds are handy in tests. */
export function memorySettingsStorage(seed: Readonly<Record<string, string>> = {}): SettingsStorage {
  const values = new Map<string, string>(Object.entries(seed));
  return {
    persistent: false,
    read: (key) => values.get(key) ?? null,
    write: (key, value) => {
      values.set(key, value);
    },
    remove: (key) => {
      values.delete(key);
    },
  };
}

/**
 * `localStorage` when it works, memory when it does not.
 *
 * The probe is a real write/read/delete round trip rather than a `typeof` check:
 * "the object exists" is not the same as "writes stick" (Safari private mode
 * throws QuotaExceededError, some enterprise policies fail after the first
 * megabyte). Every access is wrapped — a storage failure must never take the app
 * down, it must only mean "this visit is not remembered".
 */
export function browserSettingsStorage(probeKey = 'wing.web.__probe'): SettingsStorage {
  const memory = memorySettingsStorage();
  let available: boolean;
  try {
    const storage = globalThis.localStorage;
    storage.setItem(probeKey, '1');
    available = storage.getItem(probeKey) === '1';
    storage.removeItem(probeKey);
  } catch {
    available = false;
  }
  if (!available) {
    return memory;
  }
  return {
    persistent: true,
    read(key) {
      try {
        return globalThis.localStorage.getItem(key);
      } catch {
        return null;
      }
    },
    write(key, value) {
      try {
        globalThis.localStorage.setItem(key, value);
      } catch (error) {
        console.warn('could not persist the settings', error);
      }
    },
    remove(key) {
      try {
        globalThis.localStorage.removeItem(key);
      } catch (error) {
        console.warn('could not clear the settings', error);
      }
    },
  };
}
