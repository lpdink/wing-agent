/**
 * The screenshot scenes — code, not configuration (design.md D11).
 *
 * Each scene is "a fixture world + a viewport set (+ an optional interaction)".
 * Relative timestamps are computed against the run's clock so the *labels* in the
 * list ("4 min ago") are stable no matter when the scene is rendered — only the
 * absolute dates would drift, and they are not shown.
 */

import type { Page } from 'playwright';

import type { World, WorldSession } from './server';

export type ViewportId = 'desktop' | 'mobile';

export interface ViewportSpec {
  readonly id: ViewportId;
  readonly width: number;
  readonly height: number;
}

/** The two fixed viewports of the acceptance protocol. */
export const VIEWPORTS: Readonly<Record<ViewportId, ViewportSpec>> = {
  desktop: { id: 'desktop', width: 1_440, height: 900 },
  mobile: { id: 'mobile', width: 390, height: 844 },
};

export interface SceneContext {
  /** A loopback port with nothing listening (the "gateway is down" case). */
  readonly deadPort: number;
}

export interface ShotScene {
  readonly name: string;
  /** One line for the manifest: what this shot is evidence of. */
  readonly title: string;
  readonly world: World;
  readonly viewports: readonly ViewportId[];
  readonly colorScheme?: 'light' | 'dark';
  /** localStorage values to install before the app boots. */
  readonly seed?: (context: SceneContext) => Record<string, string>;
  /** Run after `ready`, before the screenshot (clicks, dialogs, …). */
  readonly interact?: (page: Page) => Promise<void>;
  /** Resolves when the UI is in the state worth photographing. */
  readonly ready: (page: Page) => Promise<unknown>;
}

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

function ago(ms: number): string {
  return new Date(Date.now() - ms).toISOString();
}

function message(
  role: string,
  content: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return { role, content, uuid: `${role}-${content.slice(0, 12)}`, ...extra };
}

/** `context_stats`: the context-window numbers the main pane shows. */
function contextStats(count: number, used: number, window: number): Record<string, unknown> {
  return {
    type: 'context_stats',
    session_id: 'wing-1',
    created_at: ago(HOUR),
    request_id: 'ctx-1',
    message_count: count,
    total_tokens: used,
    context_window_tokens: window,
    system_prompt_parts: [],
  };
}

/** `session_state_changed`: model knobs, which the replay alone does not carry. */
function stateChanged(): Record<string, unknown> {
  return {
    type: 'session_state_changed',
    session_id: 'wing-1',
    created_at: ago(HOUR),
    request_id: 'state-1',
    model: 'glm-4.6',
    model_display_name: 'GLM-4.6',
    thinking: true,
    reasoning_effort: 'high',
    yolo: false,
    title: 'Fix the parser',
    agent: null,
  };
}

const WORKSPACE = '/Users/dev/projects/wing';

function session(seed: Partial<WorldSession> & { id: string; name: string }): WorldSession {
  return {
    workspace: WORKSPACE,
    status: 'idle',
    lastInteraction: ago(HOUR),
    messages: [],
    events: [],
    info: {
      model: 'glm-4.6',
      thinking: true,
      reasoningEffort: 'high',
      yolo: false,
      workdir: WORKSPACE,
      usedTokens: 42_100,
      windowTokens: 200_000,
      messageCount: 18,
    },
    ...seed,
  };
}

/** The "a real workspace" world: four sessions, one of them mid-turn. */
const WORKING_WORLD: World = {
  sessions: [
    session({
      id: 'wing-1',
      name: 'Fix the parser',
      status: 'working',
      lastInteraction: ago(4 * MINUTE),
      messages: [
        message('user', 'The parser chokes on nested tables in the docs — can you take a look?'),
        message('assistant', 'Looking at it now: the table body is consumed as a paragraph.'),
        message('user', 'Right. Fix it without breaking the CJK wrapping.'),
      ],
      events: [contextStats(18, 42_100, 200_000), stateChanged()],
      info: {
        model: 'glm-4.6',
        thinking: true,
        reasoningEffort: 'high',
        yolo: false,
        workdir: WORKSPACE,
        usedTokens: 42_100,
        windowTokens: 200_000,
        messageCount: 18,
      },
    }),
    session({ id: 'wing-2', name: 'Deploy check', lastInteraction: ago(35 * MINUTE) }),
    session({
      id: 'wing-3',
      name: 'Rewrite the docs',
      status: 'waiting',
      lastInteraction: ago(2 * HOUR),
    }),
    session({
      id: 'wing-4',
      name: 'Nightly run',
      status: 'inactive',
      lastInteraction: ago(26 * HOUR),
    }),
  ],
};

const EMPTY_WORLD: World = { sessions: [] };

/** Settings that point at a port where nothing listens. */
function deadAddressSeed(context: SceneContext): Record<string, string> {
  return {
    'wing.web.gateway': JSON.stringify({
      version: 1,
      scheme: 'http',
      host: '127.0.0.1',
      port: context.deadPort,
      apiKey: null,
      ignoreCertErrors: false,
    }),
  };
}

export const SCENES: readonly ShotScene[] = [
  {
    name: 'sessions',
    title: 'Connected shell: session list (four sessions, one working), live session state, connection pill',
    world: WORKING_WORLD,
    viewports: ['desktop', 'mobile'],
    async ready(page) {
      await page.getByRole('button', { name: /Fix the parser/ }).waitFor();
      await page.getByText('working', { exact: true }).first().waitFor();
      await page.getByText('turn running…').waitFor();
    },
  },
  {
    name: 'sessions-dark',
    title: 'Same shell under prefers-color-scheme: dark (the theme follows the OS)',
    world: WORKING_WORLD,
    viewports: ['desktop'],
    colorScheme: 'dark',
    async ready(page) {
      await page.getByRole('button', { name: /Fix the parser/ }).waitFor();
      await page.getByText('turn running…').waitFor();
    },
  },
  {
    name: 'empty',
    title: 'Connected gateway with no sessions: the empty state and the New session path',
    world: EMPTY_WORLD,
    viewports: ['desktop'],
    async ready(page) {
      await page.getByText('No session open').first().waitFor();
      await page.getByText('No sessions yet.').waitFor();
    },
  },
  {
    name: 'settings',
    title: 'The gateway settings form: scheme/host/port/api key/ignore-cert, with the resolved URLs',
    world: WORKING_WORLD,
    viewports: ['desktop', 'mobile'],
    async ready(page) {
      await page.getByRole('button', { name: /Fix the parser/ }).waitFor();
    },
    async interact(page) {
      await page.getByRole('button', { name: 'Settings' }).click();
      await page.getByRole('dialog', { name: 'Gateway settings' }).waitFor();
    },
  },
  {
    name: 'first-connect-failure',
    title: 'A gateway that cannot be reached: the banner with the reason and the settings entry point',
    world: EMPTY_WORLD,
    viewports: ['desktop', 'mobile'],
    seed: deadAddressSeed,
    async ready(page) {
      await page.getByRole('alert').first().waitFor();
    },
  },
];
