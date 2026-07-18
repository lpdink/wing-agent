// src/core/cell-registry.tsx — Cell Registry: maps ChatItem types to Cell components.
//
// New message type = write a new Cell + one `register()` call.
// Unknown types fall back to FallbackCell (never crashes).

import type { ComponentType } from 'react'
import type { ChatItem } from '@/stores/sessionStore'
import type { CellProps } from './cell-types'

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AnyCellComponent = ComponentType<CellProps<any>>

/**
 * CellRegistry — registry pattern for message renderers.
 *
 * - `register(type, component)` — associate a ChatItem type with a Cell.
 * - `resolve(type)` — look up the Cell for a given type; returns FallbackCell if unknown.
 */
class CellRegistry {
  private cells = new Map<string, AnyCellComponent>()
  private fallback: AnyCellComponent | null = null

  /** Register a Cell component for a ChatItem type. */
  register<T extends ChatItem>(type: string, component: ComponentType<CellProps<T>>): void {
    this.cells.set(type, component as AnyCellComponent)
  }

  /** Register the fallback Cell (used for unknown types). */
  registerFallback(component: AnyCellComponent): void {
    this.fallback = component
  }

  /** Resolve the Cell component for a given ChatItem type. */
  resolve(type: string): AnyCellComponent {
    const cell = this.cells.get(type)
    if (cell) return cell
    if (this.fallback) return this.fallback
    // Last resort: render nothing (should never happen if FallbackCell is registered)
    return () => null
  }

  /** Check if a type is registered. */
  has(type: string): boolean {
    return this.cells.has(type)
  }
}

/** Singleton registry instance. */
export const registry = new CellRegistry()
