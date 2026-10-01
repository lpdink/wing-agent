/**
 * Byte accounting for the chunking layer.
 *
 * `wing/gateway/frames.py` measures payload sizes in **UTF-8 bytes** (the wire
 * unit), so every limit in the reassembly contract is a byte count as well.
 * JS strings are UTF-16, so `text.length` is not that number — `TextEncoder` is.
 */

const encoder = new TextEncoder();

/** Exact UTF-8 byte length of `text` (the number the gateway's limits speak). */
export function utf8ByteLength(text: string): number {
  return encoder.encode(text).byteLength;
}
