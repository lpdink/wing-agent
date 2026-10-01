/**
 * The web client's entry point.
 *
 * The wiring is explicit: where settings live (`localStorage`, with a memory
 * fallback), what the runtime talks to (this page's origin unless the settings say
 * otherwise), which theme tables the shared cards are drawn with (the
 * `@wing-agent/ui/styles/*` import below), and that the shell is rendered into
 * `#root`. Everything else is inside `App` / `GatewayRuntime` — the same objects
 * the tests build by hand (design.md D10).
 */

import { createRoot } from 'react-dom/client';

// The shared renderer's design tokens, imported once, here, in cascade order:
// `design-platform.css` defines the `--dsw-*` table (light on `body`, dark on
// `body[data-ds-dark-theme]`), `base.css` the supporting aliases, `scrollbar.css`
// the scrollbar skin, `focus.css` the focus ring, `shiki.css` the code palette the
// shared `CodeBlock` reads. `@wing-agent/ui/styles/*` is the package's declared
// theme seam for browser shells (its `exports` map), deliberately not pulled in by
// its barrel: the VS Code webview consumes that barrel and must keep the editor's
// own theme. Everything the *renderer's own* `--wing-*` layer needs arrives with
// the barrel itself (the package imports `styles/tokens.css` from `src/index.ts`,
// which is exactly what step 06c fixed — the alias this app used while that fix was
// pending is gone).
import '@wing-agent/ui/styles/design-platform.css';
import '@wing-agent/ui/styles/base.css';
import '@wing-agent/ui/styles/scrollbar.css';
import '@wing-agent/ui/styles/focus.css';
import '@wing-agent/ui/styles/shiki.css';

import { App } from './app/App';
import { GatewayRuntime } from './connection/runtime';
import { loadSettings, saveSettings } from './settings/settings';
import { browserSettingsStorage } from './settings/storage';
import { desktopSettingsStorage, isDesktopShell } from './settings/desktop';
import { watchColorScheme } from './theme/color-scheme';
import './ui-theme.css';
// The transcript's own rows (step 08b): the row chrome, the markdown wrapper and the
// caret the web draws itself — see the file for which declarations are transcriptions.
import './transcript/cells/cells.css';
import './styles.css';

// Before React mounts: the token tables' dark half keys on a body attribute (not a
// media query), so the first painted card is already the right scheme.
watchColorScheme();

// Settings storage: when running inside the Electron shell, settings are read
// and written through the preload bridge (IPC → main process config.json).
// In a plain browser, localStorage is used with a memory fallback.
const storage = isDesktopShell() ? desktopSettingsStorage() : browserSettingsStorage();
const runtime = new GatewayRuntime({
  initialSettings: loadSettings(storage),
  onSettingsChange: (settings) => {
    saveSettings(storage, settings);
  },
  location: { origin: globalThis.location.origin },
  // The one DOM fact the runtime cannot ask for itself: a hidden tab polls slower
  // and badges a turn that finished while nobody was looking (design.md D6).
  isPageVisible: () => globalThis.document.visibilityState !== 'hidden',
});

const container = document.getElementById('root');
if (container === null) {
  throw new Error('the #root element is missing from index.html');
}

createRoot(container).render(<App runtime={runtime} settingsPersistent={storage.persistent} />);
