import type { SessionCompactTrigger, SessionMessage } from 'claude-code'

// Below this the distill likely failed; above it the bundle would not save much context.
export const MIN_CHARS = 500
export const MAX_CHARS = 200_000
const PROBE_CHARS = 60

/**
 * Manual main-loop compactions only. Auto fires mid-task, where the narrative's elided
 * tool results lose the output that pushed context over; core's summarizer keeps it.
 */
export function shouldDistill(trigger: SessionCompactTrigger, agentId: string | undefined): boolean {
  return agentId === undefined && trigger === 'manual'
}

/** The last non-empty user text in the transcript being compacted, if any. */
export function lastUserText(messages: readonly SessionMessage[]): string | undefined {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (m?.role === 'user' && m.text.trim()) return m.text.trim()
  }
  return undefined
}

/** True when the narrative already holds the latest user ask, i.e. the transcript file was flushed. */
export function isFresh(narrative: string, lastAsk: string | undefined): boolean {
  if (lastAsk === undefined) return true
  const probe = lastAsk.split('\n').find(line => line.trim())?.trim().slice(0, PROBE_CHARS)
  return probe === undefined || narrative.includes(probe)
}

/** The single message that replaces the transcript, or null to fall back to core compaction. */
export function summaryText(sid: string, narrative: string, contextFiles: string): string | null {
  if (narrative.length < MIN_CHARS || narrative.length > MAX_CHARS) return null
  return [
    `[reseed-compact] This conversation was compacted by \`reseed distill ${sid}\`, not the built-in summarizer.`,
    `Tool calls are elided as [tool#NNN] pointers: \`reseed fetch ${sid} <NNN>\` returns one in full (defanged).`,
    'Re-read MEMORY.md and the files listed below before acting; they outrank the narrative on facts.',
    'Then continue where the narrative leaves off: the last user request in it is the live task.',
    '',
    '## Files the session touched',
    contextFiles.trim() || '(none recorded)',
    '',
    '## Narrative',
    narrative.trim(),
  ].join('\n')
}
