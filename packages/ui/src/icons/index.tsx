// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/icons/index.tsx (icon artwork),
//         packages/client/ui-primitives/src/icons/props.ts (IconProps)
// Modified for Wing: only the glyphs the ported code cards and process rows use are
// extracted — `IconCopyOutlineRegular`, `IconCheckOutlineRegular`,
// `IconWrapFillRegular`, `IconNowrapFillRegular` (batch 06b) plus
// `IconChevronDownOutlineRegular`, `IconChevronUpOutlineRegular`,
// `IconChevronLeftOutlineRegular`, `IconChevronRightOutlineRegular`,
// `IconCloseOutlineRegular`, `IconRefreshOutlineRegular`, `IconEditOutlineRegular`
// and `IconThinkOutlineRegular` (batch 06c); the shared weighted-artwork
// indirection is inlined.

/**
 * Renderer icons.
 *
 * Names identify the glyph and weight; the rendered size stays a prop. Every glyph
 * paints with `currentColor`, so the surrounding tokens drive it. This is a
 * deliberate subset of the source set: the ported cards and process rows draw
 * exactly these glyphs.
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

// ── process-row / disclosure glyphs (batch 06c) ───────────────────────────────

const IconChevronDownOutlineArtwork = ({
  size = 14,
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
      d="M4 6L7.29289 9.29289C7.68342 9.68342 8.31658 9.68342 8.70711 9.29289L12 6"
      stroke="currentColor"
    />
  </svg>
);

/** Regular one-pixel `IconChevronDownOutline` artwork. */
export const IconChevronDownOutlineRegular = (props: IconProps) => (
  <IconChevronDownOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconChevronUpOutlineArtwork = ({
  size = 14,
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
      d="M12 10L8.70711 6.70711C8.31658 6.31658 7.68342 6.31658 7.29289 6.70711L4 10"
      stroke="currentColor"
    />
  </svg>
);

/** Regular one-pixel `IconChevronUpOutline` artwork. */
export const IconChevronUpOutlineRegular = (props: IconProps) => (
  <IconChevronUpOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconChevronLeftOutlineArtwork = ({
  size = 14,
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
      d="M10 4L6.70711 7.29289C6.31658 7.68342 6.31658 8.31658 6.70711 8.70711L10 12"
      stroke="currentColor"
    />
  </svg>
);

/** Regular one-pixel `IconChevronLeftOutline` artwork. */
export const IconChevronLeftOutlineRegular = (props: IconProps) => (
  <IconChevronLeftOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconChevronRightOutlineArtwork = ({
  size = 14,
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
      d="M6 12L9.29289 8.70711C9.68342 8.31658 9.68342 7.68342 9.29289 7.29289L6 4"
      stroke="currentColor"
    />
  </svg>
);

/** Regular one-pixel `IconChevronRightOutline` artwork. */
export const IconChevronRightOutlineRegular = (props: IconProps) => (
  <IconChevronRightOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconCloseOutlineArtwork = ({
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
    <path d="M2.5 2.5L13.5 13.5" stroke="currentColor" />
    <path d="M13.5 2.5L2.5 13.5" stroke="currentColor" />
  </svg>
);

/** Regular one-pixel `IconCloseOutline` artwork. */
export const IconCloseOutlineRegular = (props: IconProps) => (
  <IconCloseOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconRefreshOutlineArtwork = ({
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
      d="M14.5001 8C14.5 9.28552 14.1188 10.5422 13.4045 11.611C12.6903 12.6799 11.6752 13.5129 10.4875 14.0049C9.29982 14.4968 7.99295 14.6255 6.73212 14.3747C5.4713 14.124 4.31314 13.505 3.4041 12.596C2.49514 11.687 1.87614 10.5288 1.62537 9.26798C1.37459 8.00716 1.50331 6.70028 1.99525 5.51261C2.48719 4.32494 3.32025 3.30981 4.3891 2.59557C5.45795 1.88134 6.71458 1.50008 8.0001 1.5C9.9001 1.5 11.7001 2.3 13.0001 3.6L14.5001 5.1"
      stroke="currentColor"
    />
    <path d="M14.4999 1.5V5.1H10.8999" stroke="currentColor" />
  </svg>
);

/** Regular one-pixel `IconRefreshOutline` artwork. */
export const IconRefreshOutlineRegular = (props: IconProps) => (
  <IconRefreshOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconEditOutlineArtwork = ({
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
      d="M8.85596 2.69971H4.19971C3.37141 2.69971 2.69992 3.37146 2.69971 4.19971V11.8003C2.69992 12.6285 3.37141 13.3003 4.19971 13.3003H11.8003C12.6283 13.2999 13.3001 12.6283 13.3003 11.8003V7.89893H14.3003V11.8003C14.3001 13.1806 13.1806 14.2999 11.8003 14.3003H4.19971C2.81913 14.3003 1.69992 13.1808 1.69971 11.8003V4.19971C1.69992 2.81918 2.81913 1.69971 4.19971 1.69971H8.85596V2.69971Z"
      fill="currentColor"
    />
    <path d="M7.7849 8.23878L13.888 2.13574" stroke="currentColor" />
  </svg>
);

/** Regular one-pixel `IconEditOutline` artwork. */
export const IconEditOutlineRegular = (props: IconProps) => (
  <IconEditOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);

const IconThinkOutlineArtwork = ({
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
      d="M10.2854 5.71481C12.9673 8.39663 14.1182 11.5938 12.8562 12.8559C11.5942 14.1179 8.39706 12.9669 5.71518 10.2851C3.03333 7.60323 1.88236 4.40608 3.14441 3.14403C4.40644 1.882 7.6036 3.03297 10.2854 5.71481Z"
      stroke="currentColor"
    />
    <path
      d="M10.2854 10.2851C7.6036 12.9669 4.40644 14.1179 3.14441 12.8559C1.88236 11.5938 3.03333 8.39663 5.71518 5.71481C8.39706 3.03297 11.5942 1.882 12.8562 3.14403C14.1182 4.40608 12.9673 7.60323 10.2854 10.2851Z"
      stroke="currentColor"
    />
    <path
      d="M8.86291 8.0002C8.86291 8.47549 8.47762 8.86087 8.00224 8.86087C7.52694 8.86087 7.1416 8.47549 7.1416 8.0002C7.1416 7.52485 7.52694 7.13953 8.00224 7.13953C8.47762 7.13953 8.86291 7.52485 8.86291 8.0002Z"
      fill="currentColor"
    />
  </svg>
);

/** Regular one-pixel `IconThinkOutline` artwork. */
export const IconThinkOutlineRegular = (props: IconProps) => (
  <IconThinkOutlineArtwork {...props} strokeWidth={ICON_REGULAR_STROKE} />
);
