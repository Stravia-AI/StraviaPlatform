/**
 * `ObservationEvent.payload` 等 `unknown` 字段的共享 narrowing。
 * 非 object 或数组一律收为空 record，读取方不再各自手写 typeof 链。
 */
export function payloadRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {}
}

export function payloadString(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined
}

export function payloadCount(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
}
