/**
 * Tests: manifest.json and PWA icon assets exist and have the right fields.
 */

import { readFileSync, statSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

const PUBLIC = resolve(__dirname, '..', 'public');
const MANIFEST_PATH = resolve(PUBLIC, 'manifest.json');

describe('manifest.json', () => {
  const manifest = JSON.parse(readFileSync(MANIFEST_PATH, 'utf8')) as Record<string, unknown>;

  it('exists and is valid JSON', () => {
    expect(manifest).toBeTruthy();
  });

  it('has the required fields', () => {
    expect(manifest['name']).toBe('Wing Agent');
    expect(manifest['short_name']).toBe('Wing');
    expect(manifest['start_url']).toBe('/');
    expect(manifest['display']).toBe('standalone');
  });

  it('has theme and background colors', () => {
    expect(typeof manifest['background_color']).toBe('string');
    expect(typeof manifest['theme_color']).toBe('string');
  });

  it('has icons array with required sizes', () => {
    const icons = manifest['icons'] as Array<{ src: string; sizes: string }>;
    expect(Array.isArray(icons)).toBe(true);
    expect(icons.length).toBeGreaterThanOrEqual(2);

    const sizes = icons.map((icon) => icon.sizes);
    expect(sizes).toContain('192x192');
    expect(sizes).toContain('512x512');
  });
});

describe('PWA icon files', () => {
  it('has icon-192.png', () => {
    const path = resolve(PUBLIC, 'icons', 'icon-192.png');
    const stats = statSync(path);
    expect(stats.isFile()).toBe(true);
    expect(stats.size).toBeGreaterThan(100);
  });

  it('has icon-512.png', () => {
    const path = resolve(PUBLIC, 'icons', 'icon-512.png');
    const stats = statSync(path);
    expect(stats.isFile()).toBe(true);
    expect(stats.size).toBeGreaterThan(100);
  });

  it('has the source SVG', () => {
    const path = resolve(PUBLIC, 'icons', 'icon.svg');
    const stats = statSync(path);
    expect(stats.isFile()).toBe(true);
  });
});

describe('index.html PWA meta', () => {
  const indexPath = resolve(__dirname, '..', 'index.html');
  const html = readFileSync(indexPath, 'utf8');

  it('links the manifest', () => {
    expect(html).toContain('href="/manifest.json"');
  });

  it('has apple-touch-icon', () => {
    expect(html).toContain('apple-touch-icon');
  });

  it('has apple-mobile-web-app-capable', () => {
    expect(html).toContain('apple-mobile-web-app-capable');
  });
});
