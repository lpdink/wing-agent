/**
 * Minimal `vscode` module mock, aliased in `vitest.config.mts`.
 *
 * Design rules:
 * - implement only what the extension host actually calls — a missing API fails
 *   loudly in tests instead of silently no-op'ing;
 * - record everything observable on {@link mockState} so tests can assert on
 *   registrations, options and log output;
 * - stay dependency-free (no node builtins beyond nothing) so the mock can also be
 *   loaded by the jsdom project if a component test ever needs it.
 */

export interface RecordedDisposable {
  readonly label: string;
  disposed: boolean;
  dispose(): void;
}

export interface RecordedOutputChannel extends RecordedDisposable {
  readonly name: string;
  readonly lines: string[];
  info(message: string): void;
  warn(message: string): void;
  error(message: string): void;
  debug(message: string): void;
  trace(message: string): void;
  show(): void;
  appendLine(message: string): void;
}

export interface RecordedViewProviderRegistration extends RecordedDisposable {
  readonly viewId: string;
  readonly provider: unknown;
  readonly options: unknown;
}

interface MockState {
  disposables: RecordedDisposable[];
  outputChannels: RecordedOutputChannel[];
  viewProviders: RecordedViewProviderRegistration[];
  commands: Map<string, (...args: unknown[]) => unknown>;
  commandCalls: { command: string; args: unknown[] }[];
  configuration: Map<string, unknown>;
  textDocumentContentProviders: Map<string, TextDocumentContentProviderLike>;
  shownDocuments: { uri: unknown; options: unknown }[];
  clipboardWrites: string[];
  reset(): void;
}

export interface TextDocumentContentProviderLike {
  provideTextDocumentContent(uri: UriLike): string;
}

export const mockState: MockState = {
  disposables: [],
  outputChannels: [],
  viewProviders: [],
  commands: new Map(),
  commandCalls: [],
  configuration: new Map(),
  textDocumentContentProviders: new Map(),
  shownDocuments: [],
  clipboardWrites: [],
  reset(): void {
    mockState.disposables = [];
    mockState.outputChannels = [];
    mockState.viewProviders = [];
    mockState.commands.clear();
    mockState.commandCalls = [];
    mockState.configuration.clear();
    mockState.textDocumentContentProviders.clear();
    mockState.shownDocuments = [];
    mockState.clipboardWrites = [];
  },
};

function makeDisposable(label: string): RecordedDisposable {
  const record: RecordedDisposable = {
    label,
    disposed: false,
    dispose(): void {
      record.disposed = true;
    },
  };
  mockState.disposables.push(record);
  return record;
}

export class Disposable {
  constructor(private readonly callOnDispose: () => void) {}

  static from(...items: { dispose(): unknown }[]): Disposable {
    return new Disposable(() => {
      for (const item of items) {
        item.dispose();
      }
    });
  }

  dispose(): void {
    this.callOnDispose();
  }
}

/** `EventEmitter` with the VS Code shape (`event` + `fire`). */
export class EventEmitter<T> {
  private readonly listeners = new Set<(event: T) => void>();

  readonly event = (listener: (event: T) => void): Disposable => {
    this.listeners.add(listener);
    return new Disposable(() => {
      this.listeners.delete(listener);
    });
  };

  fire(data: T): void {
    for (const listener of [...this.listeners]) {
      listener(data);
    }
  }

  dispose(): void {
    this.listeners.clear();
  }
}

/** URI stand-in: enough for `joinPath` + `toString` (what the host does). */
export class UriLike {
  constructor(private readonly value: string) {}

  get scheme(): string {
    return this.value.split(':')[0] ?? '';
  }

  get path(): string {
    return this.value.replace(/^[a-z]+:\/\//, '');
  }

  get fsPath(): string {
    return this.path;
  }

  toString(): string {
    return this.value;
  }

  toJSON(): string {
    return this.value;
  }
}

export const Uri = {
  file(fsPath: string): UriLike {
    return new UriLike(`file://${fsPath}`);
  },
  parse(value: string, _strict?: boolean): UriLike {
    return new UriLike(value);
  },
  joinPath(base: UriLike, ...segments: string[]): UriLike {
    return new UriLike(`${base.toString().replace(/\/$/, '')}/${segments.join('/')}`);
  },
};

/** Minimal `vscode.Position`/`vscode.Range` stand-ins (line numbers are 0-based). */
export class Position {
  constructor(
    readonly line: number,
    readonly character: number,
  ) {}
}

export class Range {
  readonly start: Position;
  readonly end: Position;

  constructor(start: Position, end: Position) {
    this.start = start;
    this.end = end;
  }
}

export const ViewColumn = { Active: -1, Beside: -2, One: 1 } as const;

export const window = {
  registerWebviewViewProvider(
    viewId: string,
    provider: unknown,
    options?: unknown,
  ): RecordedViewProviderRegistration {
    const registration = makeDisposable(`webviewViewProvider:${viewId}`) as RecordedViewProviderRegistration;
    const record = Object.assign(registration, { viewId, provider, options });
    mockState.viewProviders.push(record);
    return record;
  },

  showTextDocument(uri: unknown, options?: unknown): Promise<{ uri: unknown; options: unknown }> {
    mockState.shownDocuments.push({ uri, options });
    return Promise.resolve({ uri, options });
  },

  createOutputChannel(name: string): RecordedOutputChannel {
    const lines: string[] = [];
    const base = makeDisposable(`outputChannel:${name}`);
    const record: RecordedOutputChannel = Object.assign(base, {
      name,
      lines,
      info: (message: string) => lines.push(`info: ${message}`),
      warn: (message: string) => lines.push(`warn: ${message}`),
      error: (message: string) => lines.push(`error: ${message}`),
      debug: (message: string) => lines.push(`debug: ${message}`),
      trace: (message: string) => lines.push(`trace: ${message}`),
      show: () => undefined,
      appendLine: (message: string) => lines.push(message),
    });
    mockState.outputChannels.push(record);
    return record;
  },

  showInformationMessage: (message: string): Promise<undefined> => {
    void message;
    return Promise.resolve(undefined);
  },
  showWarningMessage: (message: string): Promise<undefined> => {
    void message;
    return Promise.resolve(undefined);
  },
  showErrorMessage: (message: string): Promise<undefined> => {
    void message;
    return Promise.resolve(undefined);
  },
};

export const commands = {
  registerCommand(command: string, callback: (...args: unknown[]) => unknown): RecordedDisposable {
    mockState.commands.set(command, callback);
    return makeDisposable(`command:${command}`);
  },
  executeCommand(command: string, ...args: unknown[]): Promise<unknown> {
    mockState.commandCalls.push({ command, args });
    return Promise.resolve(mockState.commands.get(command)?.(...args));
  },
};

/** One workspace folder (the host only reads `uri.fsPath`). */
export interface WorkspaceFolderLike {
  uri: UriLike;
  name: string;
  index: number;
}

/** `workspace` — folders, configuration (the `wing.*` settings) and content providers. */
export const workspace = {
  workspaceFolders: undefined as WorkspaceFolderLike[] | undefined,

  getConfiguration(section?: string): {
    get<T>(key: string): T | undefined;
    has(key: string): boolean;
    update(key: string, value: unknown): Promise<void>;
  } {
    const prefix = section === undefined ? '' : `${section}.`;
    return {
      get<T>(key: string): T | undefined {
        return mockState.configuration.get(`${prefix}${key}`) as T | undefined;
      },
      has(key: string): boolean {
        return mockState.configuration.has(`${prefix}${key}`);
      },
      update(key: string, value: unknown): Promise<void> {
        mockState.configuration.set(`${prefix}${key}`, value);
        return Promise.resolve();
      },
    };
  },

  registerTextDocumentContentProvider(
    scheme: string,
    provider: TextDocumentContentProviderLike,
  ): RecordedDisposable {
    mockState.textDocumentContentProviders.set(scheme, provider);
    return makeDisposable(`textDocumentContentProvider:${scheme}`);
  },
};

export const env = {
  appName: 'Visual Studio Code',
  /** URIs handed to `openExternal` (tests assert the OS-browser hand-off). */
  openedExternal: [] as unknown[],
  openExternal(uri: unknown): Promise<boolean> {
    env.openedExternal.push(uri);
    return Promise.resolve(true);
  },
  clipboard: {
    writeText(text: string): Promise<void> {
      mockState.clipboardWrites.push(text);
      return Promise.resolve();
    },
  },
};

export const version = '1.100.0';
