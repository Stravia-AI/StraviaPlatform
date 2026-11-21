import { describe, expect, test } from 'bun:test'
import { formatSpecificationTokens } from '../src/lib/model-specification'

describe('formatSpecificationTokens', () => {
  test.each([
    // Decimal K / M
    [8_000, '8K'],
    [32_000, '32K'],
    [128_000, '128K'],
    [200_000, '200K'],
    [256_000, '256K'],
    [272_000, '272K'],
    [500_000, '500K'],
    [512_000, '512K'],
    [1_000_000, '1M'],
    [1_050_000, '1.05M'],
    [2_000_000, '2M'],
    [3_500_000, '3.5M'],
    [1_500, '1.5K'],
    // Binary (1024-based) K / M
    [1_024, '1K'],
    [4_096, '4K'],
    [8_192, '8K'],
    [16_384, '16K'],
    [32_768, '32K'],
    [65_536, '64K'],
    [131_072, '128K'],
    [163_840, '160K'],
    [196_608, '192K'],
    [204_800, '200K'],
    [262_144, '256K'],
    [524_288, '512K'],
    [1_048_576, '1M'],
    [1_572_864, '1.5M'],
    [2_097_152, '2M'],
    // Exact fallback for non-aligned numbers
    [0, '0'],
    [448, '448'],
    [16_385, '16,385'],
    [1_047_576, '1,047,576'],
  ])('formats %i as %s', (value, expected) => {
    expect(formatSpecificationTokens(value)).toBe(expected)
  })
})
