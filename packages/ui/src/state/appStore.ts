import { useStore } from 'zustand';

import { createAppStore, createInitialState } from './store';
import type { AppState, AppStore } from './store';

/**
 * The app-wide store (one webview document = one store).
 *
 * Components never build state themselves; they read the mirror through
 * {@link useAppStore} and send intents through `bridge/channel.ts`.
 */
export const appStore = createAppStore();

/**
 * Reset the mirror to its empty state (tests, and the preview harness' "reload").
 *
 * `setState` merges, so the actions survive.
 */
export function resetAppStore(): void {
  appStore.setState(createInitialState());
}

/** Subscribe a component to a slice of the app state. */
export function useAppStore<T>(selector: (state: AppState) => T): T {
  return useStore(appStore, (state: AppStore) => selector(state));
}
