// A consumer shaped like the VS Code webview entry (`extensions/vscode/src/webview/main.tsx`):
// it takes the app's mount seam from the package barrel.
//
// The artifact test builds this fixture to prove the token-sheet fix did not move
// the CSS out of the webview's build by accident: `mountApp` used to be the only
// module importing the token sheet, and the webview is the one consumer that always
// loaded it.

import { mountApp, readBootstrap } from '@wing-agent/ui';

export const consumerProbe = { mountApp, readBootstrap };
