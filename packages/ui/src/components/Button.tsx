// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/Button.tsx (+ Button.module.css)
// Modified for Wing: unchanged (the `forwardRef` wrapper is kept verbatim; React 19
// accepts it and the call sites here do not need the ref-as-prop spelling).

// Button: token-styled button atom. Variants map to the --dsw-alias-button-*
// fill families; no framework imports, all behavior via props.

import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from 'react';
import clsx from 'clsx';

import css from './Button.module.css';

/** Visual variant, each backed by its --dsw-alias-button-* token family. */
export type ButtonVariant = 'primary' | 'ghost' | 'outline' | 'toolbar';

export type ButtonProps = {
  variant?: ButtonVariant;
  size?: 'md' | 'sm';
  icon?: ReactNode;
  className?: string | undefined;
  children?: ReactNode;
} & ButtonHTMLAttributes<HTMLButtonElement>;

/**
 * Render a button.
 * @param props.variant - visual family (default 'ghost').
 * @param props.size - 'md' 36px control with 12px corners or 'sm' 28px control with 8px corners.
 * @param props.icon - optional leading 16px icon node.
 * @param ref - native button for focus management and overlay anchors.
 * @returns the button element; native button attributes pass through.
 */
export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { variant = 'ghost', size = 'md', icon, className, children, ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      type="button"
      className={clsx(css.button, css[variant], css[size], className)}
      {...rest}
    >
      {icon != null && <span className={css.icon}>{icon}</span>}
      {children}
    </button>
  );
});
