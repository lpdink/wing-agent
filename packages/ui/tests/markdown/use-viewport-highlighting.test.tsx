// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/markdown/useViewportHighlighting.ts (the
//         behaviour under test); packages/client/ui-primitives/tests/
//         highlight-viewport.client.spec.tsx exists upstream for the same contract
// Modified for Wing: written against this package's `CodeBlock` and a scripted
// IntersectionObserver, so the jsdom lane covers the activation *and* the
// teardown branch the real component drives.

import { act, render } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { CodeBlock } from '../../src/markdown/CodeBlock';

/**
 * The viewport-lazy highlighting contract, as the cards use it.
 *
 * jsdom implements no `IntersectionObserver`, which is exactly why the hook has an
 * "activate immediately" arm — but that arm hides the real path. This file installs
 * a scripted observer: nothing highlights until it reports an intersection, and
 * unmounting before that must unobserve the element and leave the callback inert
 * (the branch the 06b review flagged as untested).
 */
class ScriptedIntersectionObserver implements IntersectionObserver {
  static readonly instances: ScriptedIntersectionObserver[] = [];

  readonly root = null;
  readonly rootMargin = '';
  readonly thresholds: readonly number[] = [];
  readonly observed: Element[] = [];
  readonly unobserved: Element[] = [];
  private readonly callback: IntersectionObserverCallback;

  constructor(callback: IntersectionObserverCallback) {
    this.callback = callback;
    ScriptedIntersectionObserver.instances.push(this);
  }

  observe(target: Element): void {
    this.observed.push(target);
  }

  unobserve(target: Element): void {
    this.unobserved.push(target);
  }

  disconnect(): void {
    this.unobserved.push(...this.observed);
  }

  takeRecords(): IntersectionObserverEntry[] {
    return [];
  }

  /** Report every observed element as intersecting, the way scroll-into-view would. */
  intersect(): void {
    const entries = this.observed.map(
      (target) =>
        ({
          target,
          isIntersecting: true,
          intersectionRatio: 1,
        }) as unknown as IntersectionObserverEntry,
    );
    this.callback(entries, this);
  }
}

/** The single observer the hook's module-level registry built for this test. */
function lastObserver(): ScriptedIntersectionObserver {
  const observer = ScriptedIntersectionObserver.instances.at(-1);
  if (observer === undefined) {
    throw new Error('the hook did not create an observer');
  }
  return observer;
}

beforeEach(() => {
  ScriptedIntersectionObserver.instances.length = 0;
  Object.defineProperty(globalThis, 'IntersectionObserver', {
    configurable: true,
    value: ScriptedIntersectionObserver,
  });
});

afterEach(() => {
  Object.defineProperty(globalThis, 'IntersectionObserver', {
    configurable: true,
    value: undefined,
  });
});

function renderFence(lang: string) {
  return render(
    <CodeBlock
      code={'const answer: number = 42;'}
      lang={lang}
      copyLabel="Copy"
      copiedLabel="Copied"
      toolbarLabels={{ codeLabel: 'Code', wrapLabel: 'Wrap lines', unwrapLabel: 'Do not wrap' }}
    />,
  );
}

describe('useViewportHighlighting through CodeBlock', () => {
  it('stays plain until the surface intersects, then highlights', () => {
    const view = renderFence('ts');
    expect(view.container.querySelector('pre.shiki')).toBeNull();
    expect(view.container.querySelector('[class*="plain"]')).not.toBeNull();

    act(() => {
      lastObserver().intersect();
    });

    expect(view.container.querySelector('pre.shiki')).not.toBeNull();
    expect(view.container.querySelector('[class*="plain"]')).toBeNull();
  });

  it('never observes a surface whose language the highlighter does not ship', () => {
    const view = renderFence('nosuchlang');
    expect(ScriptedIntersectionObserver.instances).toHaveLength(0);
    expect(view.container.querySelector('pre.shiki')).toBeNull();
    expect(view.container.querySelector('[class*="plain"]')).not.toBeNull();
  });

  it('unobserves on unmount and leaves a late intersection inert', () => {
    const view = renderFence('ts');
    const observer = lastObserver();
    expect(observer.observed).toHaveLength(1);

    view.unmount();
    // The hook's cleanup both unobserves the element and releases the viewport's
    // shared observer once nothing is registered with it.
    expect(observer.unobserved).toContain(observer.observed[0]);
    expect(observer.observed).toHaveLength(1);

    // A callback that arrives after teardown must not touch a dead component.
    expect(() => {
      act(() => {
        observer.intersect();
      });
    }).not.toThrow();
  });
});
