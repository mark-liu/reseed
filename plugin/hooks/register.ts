import type { EngineInterface, Register } from 'claude-code'

import { isFresh, lastUserText, shouldDistill, summaryText } from './compose.ts'

// reseed-rust is the live arm path; the cargo build lags it by months on some hosts.
async function reseedBin($: EngineInterface, home: string): Promise<string | undefined> {
  for (const bin of [`${home}/.local/bin/reseed-rust`, `${home}/.cargo/bin/reseed`]) {
    if (await $.fs.exists(bin)) return bin
  }
  return undefined
}

export const register: Register = on => {
  on('session.compact', async ($, e, next) => {
    if (!shouldDistill(e.trigger, e.agentId)) return next(e)

    // Fail open: any distill problem hands the compaction back to core.
    try {
      const home = await $.env.get('HOME')
      const bin = home ? await reseedBin($, home) : undefined
      if (!home || !bin) return next(e)

      const sid = await $.session.id()
      const out = `${home}/.claude/reseed/${sid}`
      const ran = await $.process.run([bin, 'distill', sid, '--out', out], { timeoutMs: 120_000 })
      if (ran.exitCode !== 0) return next(e)

      const narrative = await $.fs.read(`${out}/narrative.md`)
      if (!isFresh(narrative, lastUserText(e.messages))) return next(e)
      const files = (await $.fs.exists(`${out}/context-files.md`)) ? await $.fs.read(`${out}/context-files.md`) : ''
      const text = summaryText(sid, narrative, files)
      if (text === null) return next(e)

      $.ui.toast('Compacted via reseed distill')
      return { messages: [{ role: 'user', text, toolUses: [] }] }
    } catch {
      return next(e)
    }
  })
}
