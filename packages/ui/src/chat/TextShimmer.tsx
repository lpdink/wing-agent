// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/TextShimmer.tsx
// Modified for Wing: `inert` is written as the React 19 boolean attribute (the
// upstream `{...{ inert: '' }}` spread was the React 18 spelling of the same thing);
// everything else is unchanged.

/** Text activity animation shared by a row and its nested text fragments. */
import { createContext, memo, useContext, type ReactNode } from 'react';
import clsx from 'clsx';

import css from './TextShimmer.module.css';

const DecorativeCopy = createContext<boolean | undefined>(undefined);

/** Text and activity supplied by the owning row. */
export interface TextShimmerProps {
  /** Text or presentational children with text in nested TextShimmer instances. No effects or element ids. */
  children: ReactNode;
  /** Whether to animate; nested instances share the containing row's activity. */
  active?: boolean | undefined;
  className?: string | undefined;
  /** Layout class applied equally to the base and decorative content. */
  contentClassName?: string | undefined;
}

function TextContent({ children, className }: Pick<TextShimmerProps, 'children' | 'className'>) {
  const decorative = useContext(DecorativeCopy);
  const generated = decorative === true && typeof children === 'string';
  return (
    <span className={clsx(css.text, className)} data-shimmer-text={generated ? children : undefined}>
      {generated ? null : children}
    </span>
  );
}

/**
 * Render text with one shared highlight while retaining selectable, accessible content.
 * Nested instances inherit the outer animation. Keep icons outside; mark decorative
 * separators with data-shimmer-decoration so their background follows the highlight.
 *
 * While `active`, the children also render in an inert, clipped decoration: supply
 * only presentation there (no effects, no element ids).
 * @param props - localized text, running state, and owner styling.
 * @returns retained text and its optional decorative highlight.
 */
export const TextShimmer = memo(function TextShimmer({
  children,
  active = false,
  className,
  contentClassName,
}: TextShimmerProps) {
  const decorative = useContext(DecorativeCopy);
  if (decorative !== undefined) return <TextContent className={className}>{children}</TextContent>;
  const content = typeof children === 'string' ? <TextContent>{children}</TextContent> : children;
  return (
    <span className={clsx(css.root, className)} data-shimmer={active || undefined}>
      <DecorativeCopy.Provider value={false}>
        <span className={clsx(css.content, contentClassName)}>{content}</span>
      </DecorativeCopy.Provider>
      {active && (
        // React 19 assigns a boolean `inert` directly; the decoration is a screen-reader
        // and pointer no-op by construction.
        <span className={css.decoration} aria-hidden="true" inert={true}>
          <span className={css.sweep}>
            <DecorativeCopy.Provider value={true}>
              <span className={clsx(css.content, css.highlight, contentClassName)}>{content}</span>
            </DecorativeCopy.Provider>
          </span>
        </span>
      )}
    </span>
  );
});
