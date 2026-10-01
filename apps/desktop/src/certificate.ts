/**
 * Certificate policy for the shell.
 *
 * Deployment background: the gateway is exposed over **self-signed HTTPS** so a
 * phone on the same network gets a secure context. Chromium refuses those
 * certificates by default, so the shell ships an opt-in escape hatch:
 *
 * ```
 * allow ⟺ ignoreCertErrors === true  AND  the request's host (and, where the hook
 *          reports it, port) is in certificateWhitelist
 * ```
 *
 * Both halves matter. Turning the switch on alone changes nothing (an empty
 * whitelist allows nothing), and the whitelist is **port sensitive** on purpose:
 * `localhost:32523` must not bless `localhost:8443`. Entries without an explicit
 * port are rejected rather than treated as "any port on that host", so the file
 * can never silently widen into a host-level exception.
 *
 * What Electron lets each hook see decides how strict the check can be:
 *
 * - `app.on('certificate-error')` reports a full URL → exact `host:port` match;
 * - `session.setCertificateVerifyProc` reports **`hostname` only** (no port, no
 *   URL — verified against Electron 44) → host-level match against `hosts`. That
 *   is the hook the main process' own traffic actually goes through, so in
 *   practice a whitelisted host is trusted on every port of that host. Nothing in
 *   the UI should promise otherwise (see `docs/dev/web-desktop.md` §4).
 *
 * The decisions here are pure functions: `src/main.ts` wires them into those two
 * hooks, and `tests/certificate.test.ts` pins the matrix.
 */

import { isIP } from 'node:net';

/** One normalized `host:port` target, e.g. `127.0.0.1:32523` or `[::1]:8443`. */
export type CertificateTarget = string;

export interface CertificatePolicy {
  /** The user-facing switch. When `false`, nothing is ever allowed. */
  readonly ignoreCertErrors: boolean;
  /** Normalized `host:port` targets that may be trusted despite a bad certificate. */
  readonly targets: readonly CertificateTarget[];
  /**
   * Host part of every target, in **canonical comparison form**: lower case and
   * without IPv6 brackets, because that is what `setCertificateVerifyProc` reports
   * (`::1`, never `[::1]`). Comparison goes through `canonicalHost`, so a caller
   * passing the bracketed spelling still matches. Every hook that *does* see a URL
   * (`certificate-error`) stays port-exact via `targets`.
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
 * Canonical comparison form for a host: lower case, IPv6 without brackets.
 *
 * Electron reports `::1` (not `[::1]`) to `setCertificateVerifyProc`, while the
 * config file — like every URL spelling — uses the bracketed form. Folding both
 * sides through this helper is what makes an IPv6 whitelist entry actually work.
 */
export function canonicalHost(host: string): string {
  const trimmed = host.trim().toLowerCase();
  return trimmed.startsWith('[') && trimmed.endsWith(']') ? trimmed.slice(1, -1) : trimmed;
}

/** `::1:8443` (IPv6 written without brackets) → `[::1]:8443`; `null` for any other shape. */
function bracketBareIpv6Authority(authority: string): string | null {
  const match = /^(?<host>[^[\]/?@]+):(?<port>\d{1,5})$/u.exec(authority);
  const host = match?.groups?.['host'];
  const port = match?.groups?.['port'];
  if (host === undefined || port === undefined || isIP(host) !== 6) {
    return null;
  }
  return `[${host}]:${port}`;
}

/**
 * Normalize one whitelist entry to `host:port`, or `null` when it is not usable.
 *
 * Accepted shapes: `host:port`, `host:port/path`, `https://host:port`, `[::1]:8443`
 * (and the unbracketed `::1:8443`). Rejected: anything without an explicit port
 * (`localhost`), wildcards (`*:8443` — matching is exact, a literal `*` would be an
 * entry that can never fire), URLs with userinfo, non-numeric / out-of-range ports.
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
  const candidate = authority.startsWith('[')
    ? authority
    : (bracketBareIpv6Authority(authority) ?? authority);
  try {
    const parsed = new URL(`https://${candidate}`);
    if (parsed.port === '' || parsed.hostname === '' || parsed.hostname.includes('*')) {
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

/** `127.0.0.1:32523` → `127.0.0.1`, `[::1]:8443` → `::1` (see `canonicalHost`). */
function hostOfTarget(target: CertificateTarget): string {
  const separator = target.lastIndexOf(':');
  return canonicalHost(separator > 0 ? target.slice(0, separator) : target);
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
 * `hostname` — see the note on `CertificatePolicy.hosts`. Both sides go through
 * `canonicalHost` so `::1` (Electron's spelling) and `[::1]` (the file's) match.
 * Host-level matching is the narrowest check that API allows; the whitelist file
 * still demands an explicit port per entry, so a host is only reachable here by
 * being named.
 */
export function allowsIgnoringCertificateHost(hostname: string, policy: CertificatePolicy): boolean {
  if (!policy.ignoreCertErrors) {
    return false;
  }
  const host = canonicalHost(hostname);
  return host !== '' && policy.hosts.includes(host);
}
