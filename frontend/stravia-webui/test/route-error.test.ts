import { describe, expect, test } from 'bun:test'

import { classifyRouteError } from '../src/lib/route-error'

describe('classifyRouteError', () => {
  test.each([
    'Failed to fetch dynamically imported module: http://127.0.0.1:4173/_app/immutable/nodes/5.abc.js',
    'error loading dynamically imported module: http://127.0.0.1/_app/immutable/nodes/5.abc.js',
    'Importing a module script failed.',
  ])('reports a failed page resource as module_load: %s', (message) => {
    expect(classifyRouteError(new TypeError(message))).toBe('module_load')
  })

  test.each(['Failed to fetch', 'NetworkError when attempting to fetch resource.', 'Load failed'])(
    'reports a failed network request as network: %s',
    (message) => {
      expect(classifyRouteError(new TypeError(message))).toBe('network')
    },
  )

  test('never surfaces unrecognised errors, which may carry addresses or credentials', () => {
    expect(classifyRouteError(new Error('connect ECONNREFUSED postgres://admin:hunter2@10.0.0.5/db'))).toBeUndefined()
    expect(classifyRouteError(new Error('Failed to fetch'))).toBeUndefined()
    expect(classifyRouteError('Failed to fetch dynamically imported module')).toBeUndefined()
    expect(classifyRouteError(undefined)).toBeUndefined()
  })
})
