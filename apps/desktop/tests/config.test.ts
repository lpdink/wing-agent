import { mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import {
  CONFIG_FILE_NAME,
  DEFAULT_CONFIG,
  DEFAULT_GATEWAY_BASE_URL,
  configFileFor,
  loadConfig,
  mergeConfig,
  normalizeConfig,
  serializeConfig,
  writeConfig,
} from '../src/config';

/**
 * `config.json` is the shell's only persistent state and the input to the
 * certificate policy, so every normalization rule (and every degradation path)
 * is pinned here. `loadConfig` must never throw: a broken file has to leave the
 * app usable with defaults.
 */

const temps: string[] = [];

function tempDir(): string {
  const directory = mkdtempSync(path.join(tmpdir(), 'wing-desktop-config-'));
  temps.push(directory);
  return directory;
}

afterEach(() => {
  while (temps.length > 0) {
    const directory = temps.pop();
    if (directory !== undefined) {
      rmSync(directory, { recursive: true, force: true });
    }
  }
});

describe('normalizeConfig', () => {
  it('returns the documented defaults without complaints for an empty object', () => {
    expect(normalizeConfig({})).toEqual({ config: DEFAULT_CONFIG, issues: [] });
    expect(DEFAULT_CONFIG.gatewayBaseUrl).toBe(DEFAULT_GATEWAY_BASE_URL);
    expect(DEFAULT_CONFIG.ignoreCertErrors).toBe(false);
    expect(DEFAULT_CONFIG.certificateWhitelist).toEqual([]);
    expect(DEFAULT_CONFIG.apiKey).toBeNull();
    expect(DEFAULT_CONFIG.wingPath).toBeNull();
    expect(DEFAULT_CONFIG.autoStart).toBe(true);
  });

  it('rejects a non-object payload instead of guessing', () => {
    for (const raw of [null, 42, 'config', [1, 2]]) {
      const { config, issues } = normalizeConfig(raw);
      expect(config).toEqual(DEFAULT_CONFIG);
      expect(issues).toHaveLength(1);
    }
  });

  it('normalizes the gateway URL and strips trailing slashes', () => {
    expect(normalizeConfig({ gatewayBaseUrl: ' https://127.0.0.1:32523/ ' }).config.gatewayBaseUrl).toBe(
      'https://127.0.0.1:32523',
    );
    expect(normalizeConfig({ gatewayBaseUrl: 'http://host:8080/prefix/' }).config.gatewayBaseUrl).toBe(
      'http://host:8080/prefix',
    );
  });

  it('falls back to the default gateway URL for values that are not absolute http(s)', () => {
    for (const value of ['', '   ', 'not a url', 'ftp://host:21', 42, null]) {
      const { config, issues } = normalizeConfig({ gatewayBaseUrl: value });
      expect(config.gatewayBaseUrl).toBe(DEFAULT_GATEWAY_BASE_URL);
      expect(issues.some((issue) => issue.startsWith('gatewayBaseUrl'))).toBe(true);
    }
  });

  it('treats a blank API key as "no auth"', () => {
    expect(normalizeConfig({ apiKey: '  secret ' }).config.apiKey).toBe('secret');
    expect(normalizeConfig({ apiKey: '   ' }).config.apiKey).toBeNull();
    expect(normalizeConfig({ apiKey: null }).config.apiKey).toBeNull();
    const { config, issues } = normalizeConfig({ apiKey: 1234 });
    expect(config.apiKey).toBeNull();
    expect(issues).toContain('apiKey must be a string or null');
  });

  it('normalizes the certificate whitelist and reports the entries it drops', () => {
    const { config, issues } = normalizeConfig({
      certificateWhitelist: [
        '127.0.0.1:32523',
        'https://wing.local:8443/path',
        'localhost', // no port: rejected on purpose (see certificate.ts)
        '',
        'host:not-a-port',
        '127.0.0.1:32523', // duplicate
        42,
      ],
    });
    expect(config.certificateWhitelist).toEqual(['127.0.0.1:32523', 'wing.local:8443']);
    // One issue per dropped entry: `localhost`, '', 'host:not-a-port' and the number.
    expect(issues.filter((issue) => issue.startsWith('certificateWhitelist'))).toHaveLength(4);

    const notAnArray = normalizeConfig({ certificateWhitelist: 'host:1234' });
    expect(notAnArray.config.certificateWhitelist).toEqual([]);
    expect(notAnArray.issues).toContain('certificateWhitelist must be an array of "host:port" strings');
  });

  it('falls back for non-boolean switches and non-string paths', () => {
    const { config, issues } = normalizeConfig({
      ignoreCertErrors: 'yes',
      autoStart: 0,
      wingPath: 12,
    });
    expect(config.ignoreCertErrors).toBe(false);
    expect(config.autoStart).toBe(true);
    expect(config.wingPath).toBeNull();
    // Issues are collected in field order (see `normalizeConfig`).
    expect(issues).toEqual([
      'ignoreCertErrors must be a boolean',
      'wingPath must be a string or null',
      'autoStart must be a boolean',
    ]);
    expect(normalizeConfig({ wingPath: ' /usr/local/bin/wing ' }).config.wingPath).toBe(
      '/usr/local/bin/wing',
    );
    expect(normalizeConfig({ wingPath: '  ' }).config.wingPath).toBeNull();
  });

  it('ignores unknown keys (forward compatible)', () => {
    const { config, issues } = normalizeConfig({ theme: 'dark', gatewayBaseUrl: 'http://a:1' });
    expect(issues).toEqual([]);
    expect(config.gatewayBaseUrl).toBe('http://a:1');
  });
});

describe('mergeConfig', () => {
  it('applies a patch on top of the current values', () => {
    const base = normalizeConfig({ apiKey: 'first' }).config;
    const { config, issues } = mergeConfig(base, { apiKey: 'second', ignoreCertErrors: true });
    expect(issues).toEqual([]);
    expect(config.apiKey).toBe('second');
    expect(config.ignoreCertErrors).toBe(true);
    expect(config.gatewayBaseUrl).toBe(base.gatewayBaseUrl);
  });

  it('lets a patch clear the API key and replace the whitelist wholesale', () => {
    const base = normalizeConfig({
      apiKey: 'first',
      certificateWhitelist: ['a:1'],
    }).config;
    const { config } = mergeConfig(base, { apiKey: null, certificateWhitelist: ['b:2'] });
    expect(config.apiKey).toBeNull();
    expect(config.certificateWhitelist).toEqual(['b:2']);
  });

  it('reports a non-object patch without touching the base', () => {
    const base = normalizeConfig({ apiKey: 'first' }).config;
    const { config, issues } = mergeConfig(base, 'nope');
    expect(config).toEqual(base);
    expect(issues).toEqual(['the settings patch must be a JSON object']);
  });
});

describe('serializeConfig', () => {
  it('writes a stable, human-editable document', () => {
    expect(serializeConfig(DEFAULT_CONFIG)).toBe(
      `${JSON.stringify(
        {
          gatewayBaseUrl: DEFAULT_GATEWAY_BASE_URL,
          apiKey: null,
          ignoreCertErrors: false,
          certificateWhitelist: [],
          wingPath: null,
          autoStart: true,
        },
        null,
        2,
      )}\n`,
    );
  });
});

describe('loadConfig / writeConfig', () => {
  it('treats a missing file as a first run, not an error', async () => {
    const file = configFileFor(tempDir());
    const result = await loadConfig(file);
    expect(result.status).toBe('missing');
    expect(result.issues).toEqual([]);
    expect(result.config).toEqual(DEFAULT_CONFIG);
    expect(CONFIG_FILE_NAME).toBe('config.json');
  });

  it('degrades to defaults for unparsable or non-object content', async () => {
    const directory = tempDir();
    const file = configFileFor(directory);

    writeFileSync(file, '{ not json');
    const broken = await loadConfig(file);
    expect(broken.status).toBe('invalid');
    expect(broken.config).toEqual(DEFAULT_CONFIG);
    expect(broken.issues[0]).toContain('not valid JSON');

    writeFileSync(file, '"just a string"');
    const wrongShape = await loadConfig(file);
    expect(wrongShape.status).toBe('invalid');
    expect(wrongShape.issues[0]).toContain('must contain a JSON object');
  });

  it('reads a real file and reports the fields it repaired', async () => {
    const file = configFileFor(tempDir());
    writeFileSync(file, JSON.stringify({ gatewayBaseUrl: 'http://127.0.0.1:41234', apiKey: '' }));
    const result = await loadConfig(file);
    expect(result.status).toBe('loaded');
    expect(result.issues).toEqual([]);
    expect(result.config.gatewayBaseUrl).toBe('http://127.0.0.1:41234');
    expect(result.config.apiKey).toBeNull();
  });

  it('round-trips through an atomic write with 0600 permissions and no leftovers', async () => {
    const directory = tempDir();
    const file = configFileFor(directory);
    const config = normalizeConfig({
      gatewayBaseUrl: 'https://wing.local:8443',
      apiKey: 'secret',
      ignoreCertErrors: true,
      certificateWhitelist: ['wing.local:8443'],
      wingPath: '/usr/local/bin/wing',
      autoStart: false,
    }).config;

    await writeConfig(file, config);

    const reloaded = await loadConfig(file);
    expect(reloaded.status).toBe('loaded');
    expect(reloaded.config).toEqual(config);
    expect(readFileSync(file, 'utf8').endsWith('\n')).toBe(true);
    expect(statSync(file).mode & 0o777).toBe(0o600);

    await writeConfig(file, config);
    expect(readdirSync(directory)).toEqual([CONFIG_FILE_NAME]);
  });

  it('creates the user-data directory on demand', async () => {
    const directory = tempDir();
    const file = configFileFor(path.join(directory, 'nested', 'user-data'));
    await writeConfig(file, DEFAULT_CONFIG);
    expect(statSync(file).isFile()).toBe(true);
  });
});
