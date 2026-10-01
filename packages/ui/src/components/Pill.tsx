// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/Pill.tsx (+ Pill.module.css)
// Modified for Wing: the module comment now points at this package; no code change.

// Pill: capsule at the 24px text-line size, selectable when given `onClick`
// (view switcher tabs, filters) and a static span otherwise — TerminalBlock's
// exit status is the read-only case.

import clsx from 'clsx';
import type { ButtonHTMLAttributes, ReactNode } from 'react';

import css from './Pill.module.css';

/**
 * Render a pill chip. Interactive when onClick is supplied (renders a button);
 * otherwise a static span.
 * @param props.active - selected/active visual state.
 * @returns pill element.
 */
export function Pill({
  active = false,
  className,
  children,
  onClick,
  ...rest
}: {
  active?: boolean;
  // `| undefined` so a caller can forward an optional class straight through
  // under exactOptionalPropertyTypes (a CSS-module lookup is string|undefined).
  className?: string | undefined;
  children?: ReactNode;
} & ButtonHTMLAttributes<HTMLButtonElement>) {
  if (!onClick) {
    return <span className={clsx(css.pill, active && css.active, className)}>{children}</span>;
  }
  return (
    <button
      type="button"
      className={clsx(css.pill, css.interactive, active && css.active, className)}
      onClick={onClick}
      {...rest}
    >
      {children}
    </button>
  );
}
