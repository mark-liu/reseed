import { expect, test } from 'claude-code/testing'

import { MAX_CHARS, isFresh, lastUserText, shouldDistill, summaryText } from './compose.ts'

test('only main-loop manual compactions distill', async () => {
  expect(shouldDistill('manual', undefined)).toBe(true)
  expect(shouldDistill('auto', undefined)).toBe(false)
  expect(shouldDistill('precompute', undefined)).toBe(false)
  expect(shouldDistill('plugin', undefined)).toBe(false)
  expect(shouldDistill('manual', 'agent-1')).toBe(false)
})

test('summary carries the files, the resume line and the narrative', async () => {
  const narrative = 'n'.repeat(600)
  const text = summaryText('abc', narrative, '- a.md\n')
  expect(text).toContain('reseed distill abc')
  expect(text).toContain('- a.md')
  expect(text).toContain('continue where the narrative leaves off')
  expect(text?.endsWith(narrative)).toBe(true)
})

test('out-of-range narratives fall back to core', async () => {
  expect(summaryText('abc', 'short', '')).toBeNull()
  expect(summaryText('abc', 'x'.repeat(MAX_CHARS + 1), '')).toBeNull()
})

test('freshness: the latest user ask must be in the narrative', async () => {
  const messages = [
    { role: 'user' as const, text: 'first ask', toolUses: [] },
    { role: 'assistant' as const, text: 'ok', toolUses: [] },
    { role: 'user' as const, text: '\nbuild the band next\nmore', toolUses: [] },
  ]
  const ask = lastUserText(messages)
  expect(ask).toBe('build the band next\nmore')
  expect(isFresh('... **user:** build the band next ...', ask)).toBe(true)
  expect(isFresh('... **user:** first ask ...', ask)).toBe(false)
  expect(isFresh('anything', undefined)).toBe(true)
})
