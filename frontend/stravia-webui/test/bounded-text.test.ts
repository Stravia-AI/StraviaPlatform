import { describe, expect, test } from 'bun:test'
import { boundedHead, boundedTail } from '../src/lib/bounded-text'

describe('bounded text', () => {
  test('never splits a surrogate pair or grapheme at the cut', () => {
    expect(boundedHead('ab😀cd', 3)).toBe('ab')
    expect(boundedTail('ab😀cd', 3)).toBe('cd')
    expect(boundedTail('short', 100)).toBe('short')
  })
})
