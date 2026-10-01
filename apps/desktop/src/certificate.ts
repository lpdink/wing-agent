/**
 * Certificate policy for the shell.
 *
 * Deployment background: the gateway is exposed over **self-signed HTTPS** so a
 * phone on the same network gets a secure context. Chromium refuses those
 * certificates by default, so the shell ships an opt-in escape hatch:
 *
 * ```
 * allow ⟺ ignoreCertErrors === true  AND  host:port ∈ certificateWhitelist
 * ```
 *
 * Both halves matter. Turning the switch on alone changes nothing (an empty
 * whitelist allows nothing), and the whitelist is **port sensitive** on purpose:
 * `localhost:32523` must not bless `localhost:8443`. Entries without an explicit
 * port are rejected rather than treated as "any port on that host", so the file
 * can never silently widen into a host-level exception.
 *
 * The decisions here are pure functions: `src/main.ts` wires them into Electron's
 * two hooks (`certificate-error` and `session.setCertificateVerifyProc`), and
 * `tests/certificate.test.ts` pins the matrix.
 */

/** One normalized `host:port` target, e.g. `127.0.0.1:32523` or `[::1]:8443`. */
export type CertificateTarget = string;

export interface CertificatePolicy {
  /** The user-facing switch. When `false`, nothing is ever allowed. */
  readonly ignoreCertErrors: boolean;
  /** Normalized `host:port` targets that may be trusted despite a bad certificate. */
  readonly targets: readonly CertificateTarget[];
  /**
   * Host part of every target. `session.setCertificateVerifyProc` hands us only
   * `request.hostname` (no port, no URL — verified against Electron 44: the
   * request object carries `hostname`, `certificate`, `validatedCertificate`,
   * `isIssuedByKnownRoot`, `verificationResult`, `errorCode`), so that hook can
   * only match the host. Every hook that *does* see a URL (`certificate-error`)
   * stays port-exact.
   */
  readonly hosts: readonly string[];
  /** Whitelist entries that were dropped (no port, malformed, …) — surfaced for logs. */
  readonly invalidEntries: readonly string[];
}

/** Port implied by a URL scheme when the URL does not spell one out. */
function defaultPortFor(protocol: string): string | null {
  switch (protocol) {
    case 'https:':
    case 'wss:':
      return '443';
    case 'http:':
    case 'ws:':
      return '80';
    default:
      return null;
  }
}

/**
 * `host:port` key for a request URL, or `null` when the URL is unparsable or does
 * not use a scheme certificates apply to (`file:`, `wing-app:`, …).
 */
export function certificateTargetFor(url: string): CertificateTarget | null {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  const port = defaultPortFor(parsed.protocol.toLowerCase());
  if (port === null) {
    return null;
  }
  const host = parsed.hostname.toLowerCase();
  if (host === '') {
    return null;
  }
  return `${host}:${parsed.port === '' ? port : parsed.port}`;
}

/**
 * Normalize one whitelist entry to `host:port`, or `null` when it is not usable.
 *
 * Accepted shapes: `host:port`, `host:port/path`, `https://host:port`, `[::1]:8443`.
 * Rejected: anything without an explicit port (`localhost`), URLs with userinfo,
 * non-numeric / out-of-range ports.
 */
export function normalizeCertificateEntry(raw: string): CertificateTarget | null {
  if (typeof raw !== 'string') {
    return null;
  }
  const trimmed = raw.trim();
  if (trimmed === '' || trimmed.includes('@')) {
    return null;
  }
  // Keep only the authority: strip an optional scheme, then anything from `/`, `?` or `#`.
  const withoutScheme = trimmed.replace(/^[A-Za-z][A-Za-z0-9+.-]*:\/\//u, '');
  const authority = withoutScheme.split(/[/?#]/u, 1)[0] ?? '';
  if (authority === '') {
    return null;
  }
  try {
    const parsed = new URL(`https://${authority}`);
    if (parsed.port === '' || parsed.hostname === '') {
      return null;
    }
    return `${parsed.hostname.toLowerCase()}:${parsed.port}`;
  } catch {
    return null;
  }
}

/** Split raw entries into usable targets and dropped ones (keeping input order, deduplicated). */
export function normalizeCertificateWhitelist(raw: readonly unknown[]): {
  targets: CertificateTarget[];
  hosts: string[];
  invalid: string[];
} {
  const targets: CertificateTarget[] = [];
  const hosts: string[] = [];
  const invalid: string[] = [];
  for (const entry of raw) {
    const normalized = typeof entry === 'string' ? normalizeCertificateEntry(entry) : null;
    if (normalized === null) {
      invalid.push(typeof entry === 'string' ? entry : JSON.stringify(entry));
      continue;
    }
    if (targets.includes(normalized)) {
      continue;
    }
    targets.push(normalized);
    const host = hostOfTarget(normalized);
    if (!hosts.includes(host)) {
      hosts.push(host);
    }
  }
  return { targets, hosts, invalid };
}

/** `127.0.0.1:32523` → `127.0.0.1`, `[::1]:8443` → `[::1]`. */
function hostOfTarget(target: CertificateTarget): string {
  const separator = target.lastIndexOf(':');
  return separator > 0 ? target.slice(0, separator) : target;
}

/** Build the policy from the persisted config shape (accepts `unknown` so callers need no casts). */
export function createCertificatePolicy(raw: {
  readonly ignoreCertErrors?: unknown;
  readonly certificateWhitelist?: unknown;
}): CertificatePolicy {
  const whitelist = Array.isArray(raw.certificateWhitelist)
    ? normalizeCertificateWhitelist(raw.certificateWhitelist)
    : { targets: [], hosts: [], invalid: [] };
  return {
    ignoreCertErrors: raw.ignoreCertErrors === true,
    targets: whitelist.targets,
    hosts: whitelist.hosts,
    invalidEntries: whitelist.invalid,
  };
}

/**
 * The decision for Electron's `certificate-error` hook, which reports the full URL.
 * Exact `host:port` match (the strictest form we can enforce).
 */
export function allowsIgnoringCertificate(url: string, policy: CertificatePolicy): boolean {
  if (!policy.ignoreCertErrors) {
    return false;
  }
  const target = certificateTargetFor(url);
  return target !== null && policy.targets.includes(target);
}

/**
 * The decision for `session.setCertificateVerifyProc`, which only reports
 * `hostname` — see the note on `CertificatePolicy.hosts`. Host-level matching is
 * the narrowest check that API allows; the whitelist file still demands an
 * explicit port per entry, so a host is only reachable here by being named.
 */
export function allowsIgnoringCertificateHost(hostname: string, policy: CertificatePolicy): boolean {
  if (!policy.ignoreCertErrors) {
    return false;
  }
  const host = hostname.trim().toLowerCase();
  return host !== '' && policy.hosts.includes(host);
}
