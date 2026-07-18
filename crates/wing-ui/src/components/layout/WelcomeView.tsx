import { Code, Search, PenTool } from 'lucide-react'

const suggestions = [
  {
    icon: Code,
    title: 'Write code',
    description: 'Help me implement a feature',
    gradient: 'from-accent/15 to-accent/5',
  },
  {
    icon: Search,
    title: 'Debug an issue',
    description: 'Find and fix a bug in my codebase',
    gradient: 'from-success/15 to-success/5',
  },
  {
    icon: PenTool,
    title: 'Refactor',
    description: 'Improve code structure and quality',
    gradient: 'from-warning/15 to-warning/5',
  },
]

/**
 * WelcomeView — empty-state page shown when no messages exist.
 * Displays a greeting and prompt suggestion cards.
 */
export function WelcomeView() {
  return (
    <div className="flex flex-1 flex-col items-center justify-center px-6">
      {/* Greeting */}
      <div className="mb-10 text-center">
        <h2 className="mb-2 text-3xl font-bold tracking-tight text-text">
          Hey, I'm{' '}
          <span className="bg-gradient-to-r from-accent to-accent-hover bg-clip-text text-transparent">
            Wing
          </span>
        </h2>
        <p className="text-base text-text-dim">What are we building today?</p>
      </div>

      {/* Suggestion cards */}
      <div className="grid w-full max-w-xl grid-cols-3 gap-3">
        {suggestions.map((s) => {
          const Icon = s.icon
          return (
            <button
              key={s.title}
              className={`group flex flex-col items-start rounded-xl bg-gradient-to-br ${s.gradient} p-4 text-left transition-all hover:-translate-y-0.5 hover:shadow-md`}
            >
              <div className="mb-3 rounded-lg bg-bg-surface p-2 shadow-sm transition-shadow group-hover:shadow-md">
                <Icon className="h-4 w-4 text-accent" />
              </div>
              <div className="text-sm font-medium text-text">{s.title}</div>
              <div className="mt-0.5 text-xs leading-relaxed text-text-muted">{s.description}</div>
            </button>
          )
        })}
      </div>
    </div>
  )
}
