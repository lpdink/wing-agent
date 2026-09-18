import type { JsonValue } from '../../shared';

/**
 * Fault-tolerant partial JSON parser for streaming tool arguments.
 *
 * Port of `crates/wing/src/util/partial_json.rs::parse_streaming_json` — the
 * backend forwards raw `args_fragment` strings and never parses partial JSON, so
 * the frontend needs a best-effort parse to render a tool row *while the model
 * is still writing the arguments*.
 *
 * Guarantees (mirroring the Rust implementation):
 * - one O(n) pass, no exceptions and no recursion beyond {@link MAX_DEPTH};
 * - complete JSON takes the `JSON.parse` fast path and stays byte-exact;
 * - truncated input yields the fields that were already complete
 *   (`{"command": "pnpm test` → `{command: 'pnpm test'}`);
 * - garbage input yields `null` (callers treat "no object" as "nothing yet").
 */

/** Maximum nesting depth (adversarial input protection). */
const MAX_DEPTH = 128;

const WHITESPACE = new Set([' ', '\t', '\n', '\r']);

class PartialParser {
  private pos = 0;
  private depth = 0;

  constructor(private readonly text: string) {}

  parse(): JsonValue | null {
    const value = this.parseValue();
    return value === undefined ? null : value;
  }

  private peek(): string | undefined {
    return this.text[this.pos];
  }

  private advance(): string | undefined {
    const char = this.text[this.pos];
    if (char !== undefined) {
      this.pos += 1;
    }
    return char;
  }

  private skipWhitespace(): void {
    while (true) {
      const char = this.peek();
      if (char === undefined || !WHITESPACE.has(char)) {
        return;
      }
      this.pos += 1;
    }
  }

  /** Parse any value; `undefined` when the input is exhausted before a token. */
  private parseValue(): JsonValue | undefined {
    this.skipWhitespace();
    const char = this.peek();
    if (char === undefined) {
      return undefined;
    }
    switch (char) {
      case '"':
        return this.parseString();
      case '{':
        return this.parseObject();
      case '[':
        return this.parseArray();
      case 't':
        return this.parseLiteral('true', true);
      case 'f':
        return this.parseLiteral('false', false);
      case 'n':
        return this.parseLiteral('null', null);
      default:
        if (char === '-' || (char >= '0' && char <= '9')) {
          return this.parseNumber();
        }
        return undefined;
    }
  }

  private parseLiteral(word: string, value: JsonValue): JsonValue | undefined {
    for (const expected of word) {
      const char = this.advance();
      if (char === undefined) {
        return value; // truncated literal still counts as its intent
      }
      if (char !== expected) {
        return undefined;
      }
    }
    return value;
  }

  private parseNumber(): number {
    const start = this.pos;
    if (this.peek() === '-') {
      this.pos += 1;
    }
    this.consumeDigits();
    if (this.peek() === '.') {
      this.pos += 1;
      this.consumeDigits();
    }
    if (this.peek() === 'e' || this.peek() === 'E') {
      this.pos += 1;
      if (this.peek() === '+' || this.peek() === '-') {
        this.pos += 1;
      }
      this.consumeDigits();
    }
    const parsed = Number(this.text.slice(start, this.pos));
    return Number.isFinite(parsed) ? parsed : 0;
  }

  private consumeDigits(): void {
    while (true) {
      const char = this.peek();
      if (char === undefined || char < '0' || char > '9') {
        return;
      }
      this.pos += 1;
    }
  }

  /** String until the closing quote or EOF (unterminated input keeps its content). */
  private parseString(): string {
    this.advance(); // opening quote
    let result = '';
    while (true) {
      const char = this.advance();
      if (char === undefined) {
        return result;
      }
      if (char === '"') {
        return result;
      }
      if (char !== '\\') {
        result += char;
        continue;
      }
      const escaped = this.advance();
      if (escaped === undefined) {
        return result;
      }
      switch (escaped) {
        case '"':
        case '\\':
        case '/':
          result += escaped;
          break;
        case 'b':
          result += '\b';
          break;
        case 'f':
          result += '\f';
          break;
        case 'n':
          result += '\n';
          break;
        case 't':
          result += '\t';
          break;
        case 'r':
          result += '\r';
          break;
        case 'u': {
          const high = this.tryParseHex4();
          if (high === null) {
            return result; // EOF mid-escape — keep what we have
          }
          // Surrogate pair (e.g. `\uD83D\uDE00` → 😀); a lone surrogate is kept.
          if (high >= 0xd800 && high <= 0xdbff && this.text.startsWith('\\u', this.pos)) {
            const saved = this.pos;
            this.pos += 2;
            const low = this.tryParseHex4();
            if (low !== null && low >= 0xdc00 && low <= 0xdfff) {
              result += String.fromCodePoint(0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00));
            } else {
              this.pos = saved;
              result += String.fromCodePoint(high);
            }
          } else {
            result += String.fromCodePoint(high);
          }
          break;
        }
        default:
          // Invalid escape: keep backslash + char (Rust behavior).
          result += `\\${escaped}`;
      }
    }
  }

  private tryParseHex4(): number | null {
    const hex = this.text.slice(this.pos, this.pos + 4);
    if (!/^[0-9a-fA-F]{4}$/.test(hex)) {
      return null;
    }
    this.pos += 4;
    return Number.parseInt(hex, 16);
  }

  private parseObject(): JsonValue | undefined {
    if (this.depth >= MAX_DEPTH) {
      return undefined;
    }
    this.depth += 1;
    this.advance(); // `{`
    const result: Record<string, JsonValue> = {};
    while (true) {
      this.skipWhitespace();
      const char = this.peek();
      if (char === undefined) {
        this.depth -= 1;
        return result;
      }
      if (char === '}') {
        this.advance();
        this.depth -= 1;
        return result;
      }
      if (char === ',') {
        this.advance();
        continue;
      }
      if (char !== '"') {
        // A key that is not a string: malformed — keep what we parsed.
        this.depth -= 1;
        return result;
      }
      const key = this.parseString();
      this.skipWhitespace();
      if (this.peek() === ':') {
        this.advance();
      } else {
        // EOF or malformed after the key — keep the key with null (Rust parity).
        result[key] = null;
        this.depth -= 1;
        return result;
      }
      const value = this.parseValue();
      if (value === undefined) {
        result[key] = null;
        this.depth -= 1;
        return result;
      }
      result[key] = value;
    }
  }

  private parseArray(): JsonValue | undefined {
    if (this.depth >= MAX_DEPTH) {
      return undefined;
    }
    this.depth += 1;
    this.advance(); // `[`
    const result: JsonValue[] = [];
    while (true) {
      this.skipWhitespace();
      const char = this.peek();
      if (char === undefined) {
        this.depth -= 1;
        return result;
      }
      if (char === ']') {
        this.advance();
        this.depth -= 1;
        return result;
      }
      if (char === ',') {
        this.advance();
        continue;
      }
      const value = this.parseValue();
      if (value === undefined) {
        this.depth -= 1;
        return result;
      }
      result.push(value);
    }
  }
}

/**
 * Best-effort parse of a (possibly incomplete) JSON document.
 *
 * `null` when nothing usable was parsed; callers treat a non-object result the
 * same way (tool arguments are always an object).
 */
export function parsePartialJson(text: string): JsonValue | null {
  const trimmed = text.trim();
  if (trimmed === '') {
    return null;
  }
  try {
    return JSON.parse(trimmed) as JsonValue;
  } catch {
    // Fall through to the tolerant pass.
  }
  return new PartialParser(trimmed).parse();
}

/** The object view of a JSON value; `null` for arrays, scalars and `null`. */
export function asObject(value: JsonValue | null): Record<string, JsonValue> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, JsonValue>)
    : null;
}

/** The array view of a JSON value; `null` for everything else. */
export function asArray(value: JsonValue | null | undefined): readonly JsonValue[] | null {
  return Array.isArray(value) ? (value as readonly JsonValue[]) : null;
}
