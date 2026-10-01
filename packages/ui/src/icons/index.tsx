// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/icons/index.tsx (icon artwork),
//         packages/client/ui-primitives/src/icons/props.ts (IconProps)
// Modified for Wing: only the four glyphs the ported code cards use are extracted
// (`IconCopyOutlineRegular`, `IconCheckOutlineRegular`, `IconWrapFillRegular`,
// `IconNowrapFillRegular`); the shared weighted-artwork indirection is inlined.

/**
 * Code-card icons.
 *
 * Names identify the glyph and weight; the rendered size stays a prop. Every glyph
 * paints with `currentColor`, so the card's own color tokens drive it. This is a
 * deliberate subset of the source set: the ported toolbar draws exactly these four.
 */

export interface IconProps {
  /** Square edge in px; defaults to the glyph's own drawn size. */
  size?: number | undefined;
  /** Extra class for layout placement; colour rides `currentColor`. */
  className?: string | undefined;
}

/** Regular stroke width used by the product icon set. */
const ICON_REGULAR_STROKE = 1;

const IconCheckOutlineArtwork = ({
  size = 16,
  className,
  strokeWidth,
}: IconProps & { strokeWidth: number }) => (
  <svg
    width={size}
    height={size}
    className={className}
    viewBox="0 0 16 16"
    fill="none"
    xmlns="http://www.w3.org/2000/svg"
    aria-hidden="true"
    strokeWidth={strokeWidth}
  >
    <path
      d="M2.25 8.5L5.49732 11.7473C5.90519 12.1552 6.57263 12.1344 6.95426 11.7018L13.75 4"
      stroke="currentColor"
    />
  </svg>
);

/** Regular one-pixel `IconCheckOutline` artwork. */
export const IconCheckOutlineRegular = (props: IconProps) => (
  <IconCheckOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconCopyOutlineArtwork = ({
  size = 16,
  className,
  strokeWidth,
}: IconProps & { strokeWidth: number }) => (
  <svg
    width={size}
    height={size}
    className={className}
    viewBox="0 0 16 16"
    fill="none"
    xmlns="http://www.w3.org/2000/svg"
    aria-hidden="true"
    strokeWidth={strokeWidth}
  >
    <rect x="1.52075" y="4.07373" width="10.3932" height="10.3932" rx="2" stroke="currentColor" />
    <path
      d="M11.9792 1.53296C13.36 1.53296 14.4792 2.65225 14.4792 4.03296V9.42847C14.4792 10.3756 13.9521 11.1987 13.1755 11.6228V10.3298C13.3652 10.0787 13.4792 9.7674 13.4792 9.42847V4.03296C13.4792 3.20453 12.8077 2.53296 11.9792 2.53296H6.58374C6.27966 2.53301 5.99684 2.6235 5.7605 2.77905H4.42358C4.85652 2.03463 5.66056 1.53304 6.58374 1.53296H11.9792Z"
      fill="currentColor"
    />
  </svg>
);

/** Regular one-pixel `IconCopyOutline` artwork. */
export const IconCopyOutlineRegular = (props: IconProps) => (
  <IconCopyOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconNowrapFillArtwork = ({ size = 16, className }: IconProps) => (
  <svg
    width={size}
    height={size}
    className={className}
    viewBox="0 0 16 16"
    fill="none"
    xmlns="http://www.w3.org/2000/svg"
    aria-hidden="true"
  >
    <path d="M2 15H1V1H2V15Z" fill="currentColor" />
    <path
      d="M12.3535 7.64645C12.5487 7.84171 12.5487 8.15829 12.3535 8.35355L9.85352 10.8535L9.14648 10.1465L10.793 8.5H3.5V7.5H10.793L9.14648 5.85352L9.85352 5.14648L12.3535 7.64645Z"
      fill="currentColor"
    />
    <path d="M15 15H14V1H15V15Z" fill="currentColor" />
  </svg>
);

/** Regular `IconNowrapFill` artwork; fill-only weights render identically. */
export const IconNowrapFillRegular = (props: IconProps) => <IconNowrapFillArtwork {...props} />;

const IconWrapFillArtwork = ({ size = 16, className }: IconProps) => (
  <svg
    width={size}
    height={size}
    className={className}
    viewBox="0 0 16 16"
    fill="none"
    xmlns="http://www.w3.org/2000/svg"
    aria-hidden="true"
  >
    <path
      d="M10.9999 8C10.9999 6.89543 10.1046 6 9 6H4.5V5H9C10.6568 5 11.9999 6.34315 11.9999 8C11.9999 9.65685 10.6568 11 9 11H6.20703L6.85351 11.6465L6.14648 12.3535L4.64652 10.8536C4.45126 10.6583 4.45126 10.3417 4.64652 10.1464L6.14648 8.64648L6.85351 9.35352L6.20703 10H9C10.1046 10 10.9999 9.10457 10.9999 8Z"
      fill="currentColor"
    />
    <path d="M2 15H1V1H2V15Z" fill="currentColor" />
    <path d="M15 15H14V1H15V15Z" fill="currentColor" />
  </svg>
);

/** Regular `IconWrapFill` artwork; fill-only weights render identically. */
export const IconWrapFillRegular = (props: IconProps) => <IconWrapFillArtwork {...props} />;
