import { WelcomeView } from './WelcomeView'

/**
 * ChatArea — scrollable message container.
 *
 * Phase 3: always shows WelcomeView (no real messages yet).
 * Phase 5: will render Cell components via CellRegistry.
 */
export function ChatArea() {
  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
      <WelcomeView />
    </div>
  )
}
