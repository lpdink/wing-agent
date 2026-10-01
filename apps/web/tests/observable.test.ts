import { describe, expect, it, vi } from 'vitest';

import { Notifier } from '../src/lib/observable';

describe('Notifier', () => {
  it('wakes every subscriber and forgets unsubscribed ones', () => {
    const notifier = new Notifier();
    const first = vi.fn();
    const second = vi.fn();
    const unsubscribe = notifier.subscribe(first);
    notifier.subscribe(second);

    notifier.notify();
    expect(first).toHaveBeenCalledTimes(1);
    expect(second).toHaveBeenCalledTimes(1);
    expect(notifier.listenerCount).toBe(2);

    unsubscribe();
    notifier.notify();
    expect(first).toHaveBeenCalledTimes(1);
    expect(second).toHaveBeenCalledTimes(2);
    expect(notifier.listenerCount).toBe(1);
  });

  it('is idempotent on unsubscribe', () => {
    const notifier = new Notifier();
    const listener = vi.fn();
    const unsubscribe = notifier.subscribe(listener);
    unsubscribe();
    unsubscribe();
    notifier.notify();
    expect(listener).not.toHaveBeenCalled();
  });

  it('contains a throwing listener (and still reports it)', () => {
    const notifier = new Notifier();
    const error = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const broken = vi.fn(() => {
      throw new Error('boom');
    });
    const healthy = vi.fn();
    notifier.subscribe(broken);
    notifier.subscribe(healthy);

    expect(() => {
      notifier.notify();
    }).not.toThrow();
    expect(healthy).toHaveBeenCalledTimes(1);
    expect(error).toHaveBeenCalled();
    error.mockRestore();
  });

  it('lets a listener unsubscribe itself while notifying', () => {
    const notifier = new Notifier();
    const calls: string[] = [];
    const unsubscribe = notifier.subscribe(() => {
      calls.push('self');
      unsubscribe();
    });
    notifier.subscribe(() => {
      calls.push('other');
    });

    notifier.notify();
    notifier.notify();
    expect(calls).toEqual(['self', 'other', 'other']);
  });
});
