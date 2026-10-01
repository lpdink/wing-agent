/**
 * Settings → the gateway the image resolver asks.
 *
 * The one place that knows both sides: the renderer's need ("which session's
 * workspace, at which base URL, with which key") and the app's settings model
 * (`host: ''` = this page's origin, `https` ⇒ the base URL follows). Failures are
 * `null`, not throws: an address the settings cannot produce means *no images*, and
 * the settings dialog already explains that state (step 07, D9).
 */

import type { GatewaySettings } from '../settings/settings';
import { GatewayAddressError, gatewayEndpoints, type PageLocation } from '../settings/urls';

import type { ImageTarget } from './resolver';

export function imageTarget(input: {
  readonly settings: GatewaySettings;
  readonly location: PageLocation;
  readonly sessionId: string | null;
}): ImageTarget | null {
  if (input.sessionId === null) {
    return null;
  }
  try {
    const endpoints = gatewayEndpoints(input.settings, input.location);
    return {
      baseUrl: endpoints.httpBaseUrl,
      apiKey: input.settings.apiKey,
      sessionId: input.sessionId,
    };
  } catch (error) {
    if (error instanceof GatewayAddressError) {
      return null;
    }
    throw error;
  }
}
