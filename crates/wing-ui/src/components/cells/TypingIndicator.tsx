// src/components/cells/TypingIndicator.tsx — Agent working animation.
//
// Shows three bouncing dots when agent is processing but hasn't produced text yet.

export function TypingIndicator() {
  return (
    <div className="flex justify-start">
      <div className="flex items-center gap-1.5 rounded-full bg-bg-elevated px-4 py-2.5">
        <span
          className="typing-dot h-2 w-2 rounded-full bg-text-muted"
          style={{ animationDelay: '0ms' }}
        />
        <span
          className="typing-dot h-2 w-2 rounded-full bg-text-muted"
          style={{ animationDelay: '150ms' }}
        />
        <span
          className="typing-dot h-2 w-2 rounded-full bg-text-muted"
          style={{ animationDelay: '300ms' }}
        />
      </div>
    </div>
  )
}
