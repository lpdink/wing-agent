/**
 * The web client's entry point.
 *
 * Three lines of wiring, all of them explicit: where settings live
 * (`localStorage`, with a memory fallback), what the runtime talks to (this page's
 * origin unless the settings say otherwise), and that the shell is rendered into
 * `#root`. Everything else is inside `App` / `GatewayRuntime` — the same objects
 * the tests build by hand (design.md D10).
 */

import { createRoot } from 'react-dom/client';

import { App } from './app/App';
import { GatewayRuntime } from './connection/runtime';
import { loadSettings, saveSettings } from './settings/settings';
import { browserSettingsStorage } from './settings/storage';
import './ui-theme.css';
import './styles.css';

const storage = browserSettingsStorage();
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
