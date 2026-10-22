import { expect, test } from 'bun:test'

import { inlineSaveKeyAction, reconcileInlineDraft } from '../src/lib/inline-save'

test('Enter saves a changed value and Escape restores it', () => {
  expect(inlineSaveKeyAction({ key: 'Enter', isComposing: false }, { dirty: true, saving: false })).toBe('save')
  expect(inlineSaveKeyAction({ key: 'Escape', isComposing: false }, { dirty: true, saving: false })).toBe('revert')
})

test('keys are ignored without a change, while saving, or during IME composition', () => {
  expect(inlineSaveKeyAction({ key: 'Enter', isComposing: false }, { dirty: false, saving: false })).toBeNull()
  expect(inlineSaveKeyAction({ key: 'Escape', isComposing: false }, { dirty: false, saving: false })).toBeNull()
  expect(inlineSaveKeyAction({ key: 'Enter', isComposing: false }, { dirty: true, saving: true })).toBeNull()
  expect(inlineSaveKeyAction({ key: 'Escape', isComposing: false }, { dirty: true, saving: true })).toBeNull()
  expect(inlineSaveKeyAction({ key: 'Enter', isComposing: true }, { dirty: true, saving: false })).toBeNull()
  expect(inlineSaveKeyAction({ key: 'a', isComposing: false }, { dirty: true, saving: false })).toBeNull()
})

test('an untouched draft follows the saved value while an edited draft is kept', () => {
  expect(reconcileInlineDraft({ draft: '30', previousSaved: '30', nextSaved: '45' })).toBe('45')
  expect(reconcileInlineDraft({ draft: '31', previousSaved: '30', nextSaved: '45' })).toBe('31')
  expect(reconcileInlineDraft({ draft: '30', previousSaved: '30', nextSaved: '30' })).toBe('30')
})
