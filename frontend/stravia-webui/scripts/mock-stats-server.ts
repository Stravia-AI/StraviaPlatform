import { stat } from 'node:fs/promises'
import { resolve, sep } from 'node:path'

// 用量页的 mock 服务：同一个进程既托管 `dist/` 静态构建，又在 `/api/v1` 下按真实后端的契约
// 提供用量统计接口，用来在真实浏览器里手动或脚本化地验证筛选、悬停和动画。
//
// 用法（先 `bun run build`）：
//   bun scripts/mock-stats-server.ts [--port 4180] [--host 127.0.0.1] [--delay-ms 0] [--seed 1]
//
// 数据由确定性伪随机生成：同一个 15 分钟桶在任何窗口、任何维度下数值固定，
// Provider / API Key 按各自的边际口径汇总，和后端 `stats_usage_buckets` 的口径一致：
//   - Provider 筛选：请求数 = 上游尝试数；终结 / 失败 / 恢复四个分类列恒为 0；
//   - API Key 筛选：请求数 = 自身请求数；上游尝试与分类列恒为 0，无订阅摊销。
//
// 运行期控制（便于观察占位淡化、错误面板）：
//   GET|POST /__mock/config   {"delayMs": 800, "failStats": true}
//   GET|DELETE /__mock/requests   读取 / 清空 `/api/v1/stats/*` 请求记录

const BASE_MS = 900_000
const DAY_MS = 86_400_000
const HISTORY_DAYS = 45
const MAX_BUCKETS = 512
const MAX_FILTER_IDS = 100
const USD = { kind: 'usd', symbol: '$' } as const

const webuiRoot = resolve(import.meta.dir, '..')
const dist = resolve(webuiRoot, 'dist')
const indexPath = resolve(dist, 'index.html')

function flagValue(name: string): string | undefined {
  const index = process.argv.indexOf(name)
  return index >= 0 ? process.argv[index + 1] : undefined
}
const port = Number(flagValue('--port') ?? 4180)
const host = flagValue('--host') ?? '127.0.0.1'
const seed = Number(flagValue('--seed') ?? 1)
const config = { delayMs: Number(flagValue('--delay-ms') ?? 0), failStats: false }

// ---------------------------------------------------------------- 实体

interface Provider {
  id: string | null
  name: string
  deleted?: boolean
  weight: number
  /** 上游尝试的基础错误率；`spike` 为每天固定时段内叠加的故障。 */
  errorRate: number
  spike?: { startHour: number; hours: number; rate: number }
  durationMs: number
  firstTokenMs: number
  tps: number
  /** 每个成功请求的平均 Token；缓存为 null 表示该供应商不上报。 */
  tokens: { input: number; output: number; cacheRead: number | null; cacheWrite: number | null }
  /** 每百万 Token 的美元单价。 */
  rate: { input: number; output: number; cacheRead: number; cacheWrite: number }
  basis: 'usage' | 'subscription' | 'none'
  /** 订阅月费（美元）；仅 basis 为 subscription 时有值。 */
  monthlyPrice?: number
  /** 按量成本里无法计价的尝试占比。 */
  unpricedShare?: number
}

const PROVIDERS: Provider[] = [
  {
    id: 'prov-openai',
    name: 'OpenAI',
    weight: 0.34,
    errorRate: 0.012,
    durationMs: 4200,
    firstTokenMs: 620,
    tps: 62,
    tokens: { input: 1800, output: 420, cacheRead: 5200, cacheWrite: 260 },
    rate: { input: 2.5, output: 10, cacheRead: 0.25, cacheWrite: 3.1 },
    basis: 'usage',
  },
  {
    id: 'prov-anthropic',
    name: 'Anthropic · Claude Code Pro',
    weight: 0.28,
    errorRate: 0.02,
    durationMs: 6100,
    firstTokenMs: 980,
    tps: 48,
    tokens: { input: 2400, output: 610, cacheRead: 9800, cacheWrite: 540 },
    rate: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
    basis: 'subscription',
    monthlyPrice: 20,
  },
  {
    id: 'prov-deepseek',
    name: 'DeepSeek',
    weight: 0.16,
    errorRate: 0.03,
    spike: { startHour: 13, hours: 3, rate: 0.35 },
    durationMs: 5200,
    firstTokenMs: 1400,
    tps: 38,
    tokens: { input: 1500, output: 380, cacheRead: 4100, cacheWrite: 0 },
    rate: { input: 0.28, output: 0.42, cacheRead: 0.028, cacheWrite: 0.28 },
    basis: 'usage',
  },
  {
    id: 'prov-gemini',
    name: 'Google Gemini',
    weight: 0.12,
    errorRate: 0.015,
    durationMs: 3300,
    firstTokenMs: 540,
    tps: 88,
    tokens: { input: 2100, output: 520, cacheRead: 3000, cacheWrite: null },
    rate: { input: 1.25, output: 10, cacheRead: 0.31, cacheWrite: 0 },
    basis: 'usage',
    unpricedShare: 0.04,
  },
  {
    id: 'prov-ollama',
    name: 'Local Ollama',
    weight: 0.07,
    errorRate: 0.005,
    durationMs: 9800,
    firstTokenMs: 260,
    tps: 21,
    tokens: { input: 900, output: 300, cacheRead: null, cacheWrite: null },
    rate: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    basis: 'none',
  },
  {
    // 已删除的供应商：历史流量仍在，但没有 ID，页面不能按它筛选。
    id: null,
    name: 'Retired relay',
    deleted: true,
    weight: 0.03,
    errorRate: 0.05,
    durationMs: 7000,
    firstTokenMs: 1500,
    tps: 30,
    tokens: { input: 1000, output: 250, cacheRead: null, cacheWrite: null },
    rate: { input: 1, output: 4, cacheRead: 0.1, cacheWrite: 1 },
    basis: 'usage',
  },
]

interface ApiKey {
  id: string
  name: string
  deleted?: boolean
  weight: number
  tokenFactor: number
  latencyFactor: number
}
const API_KEYS: ApiKey[] = [
  { id: 'key-desktop', name: 'Desktop client', weight: 0.42, tokenFactor: 1, latencyFactor: 1 },
  { id: 'key-ci', name: 'CI pipeline', weight: 0.27, tokenFactor: 1.8, latencyFactor: 0.9 },
  { id: 'key-agent', name: 'Research agent', weight: 0.21, tokenFactor: 2.6, latencyFactor: 1.25 },
  { id: 'key-mobile', name: 'Mobile app', weight: 0.08, tokenFactor: 0.4, latencyFactor: 0.8 },
  // 已删除密钥的 ID 为空：页面不能勾选。
  { id: '', name: 'Deleted key', deleted: true, weight: 0.02, tokenFactor: 1, latencyFactor: 1 },
]

interface Model {
  id: string | null
  name: string
  deleted?: boolean
  weight: number
  periodHours: number
}
const MODELS: Model[] = [
  { id: 'm-gpt', name: 'gpt-5.4', weight: 0.28, periodHours: 30 },
  { id: 'm-opus', name: 'claude-opus-4.6', weight: 0.2, periodHours: 22 },
  { id: 'm-sonnet', name: 'claude-sonnet-4.5', weight: 0.2, periodHours: 41 },
  { id: 'm-deepseek', name: 'deepseek-v3.2', weight: 0.14, periodHours: 17 },
  { id: 'm-gemini', name: 'gemini-3-pro', weight: 0.1, periodHours: 26 },
  { id: 'm-llama', name: 'llama-3.3-70b', weight: 0.06, periodHours: 35 },
  { id: null, name: 'legacy-model', deleted: true, weight: 0.02, periodHours: 50 },
]

// ---------------------------------------------------------------- 伪随机

function hash(value: number): number {
  let h = (value ^ Math.imul(seed, 0x9e3779b1)) | 0
  h = Math.imul(h ^ (h >>> 16), 0x85ebca6b)
  h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35)
  return (h ^ (h >>> 16)) >>> 0
}
function rng(key: number): () => number {
  let state = hash(key) || 1
  return () => {
    state = (state + 0x6d2b79f5) | 0
    let t = Math.imul(state ^ (state >>> 15), 1 | state)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

/** 按权重和一点随机扰动把 `total` 拆成整数份，总和严格等于 `total`。 */
function split(total: number, weights: readonly number[], rand: () => number): number[] {
  const shares = weights.map((weight) => weight * (0.7 + 0.6 * rand()))
  const sum = shares.reduce((a, b) => a + b, 0)
  const exact = shares.map((share) => (total * share) / sum)
  const parts = exact.map(Math.floor)
  let rest = total - parts.reduce((a, b) => a + b, 0)
  const order = exact.map((value, index) => [value - Math.floor(value), index] as const).sort((a, b) => b[0] - a[0])
  for (const [, index] of order) {
    if (rest <= 0) break
    parts[index] += 1
    rest -= 1
  }
  return parts
}

// ---------------------------------------------------------------- 累计

type Nullable = number | null
const addNullable = (a: Nullable, b: Nullable): Nullable => (a == null ? b : b == null ? a : a + b)

interface Acc {
  requests: number
  terminal: number
  failed: number
  recovered: number
  recoveryKnown: number
  attempts: number
  errors: number
  input: Nullable
  output: Nullable
  cacheRead: Nullable
  cacheWrite: Nullable
  reasoning: Nullable
  successes: number
  durationSum: number
  firstTokenSum: number
  tpsTokens: number
  tpsSeconds: number
  /** 美元按量成本（订阅供应商为等价按量成本）。 */
  cost: number
  knownAttempts: number
  unknownAttempts: number
}
const emptyAcc = (): Acc => ({
  requests: 0,
  terminal: 0,
  failed: 0,
  recovered: 0,
  recoveryKnown: 0,
  attempts: 0,
  errors: 0,
  input: null,
  output: null,
  cacheRead: null,
  cacheWrite: null,
  reasoning: null,
  successes: 0,
  durationSum: 0,
  firstTokenSum: 0,
  tpsTokens: 0,
  tpsSeconds: 0,
  cost: 0,
  knownAttempts: 0,
  unknownAttempts: 0,
})
function merge(into: Acc, from: Acc): void {
  into.requests += from.requests
  into.terminal += from.terminal
  into.failed += from.failed
  into.recovered += from.recovered
  into.recoveryKnown += from.recoveryKnown
  into.attempts += from.attempts
  into.errors += from.errors
  into.input = addNullable(into.input, from.input)
  into.output = addNullable(into.output, from.output)
  into.cacheRead = addNullable(into.cacheRead, from.cacheRead)
  into.cacheWrite = addNullable(into.cacheWrite, from.cacheWrite)
  into.reasoning = addNullable(into.reasoning, from.reasoning)
  into.successes += from.successes
  into.durationSum += from.durationSum
  into.firstTokenSum += from.firstTokenSum
  into.tpsTokens += from.tpsTokens
  into.tpsSeconds += from.tpsSeconds
  into.cost += from.cost
  into.knownAttempts += from.knownAttempts
  into.unknownAttempts += from.unknownAttempts
}

// ---------------------------------------------------------------- 单个 15 分钟桶

interface BucketFacts {
  global: Acc
  providers: Acc[]
  keys: Acc[]
  models: Acc[]
}
const cache = new Map<number, BucketFacts | null>()

function traffic(t: number, rand: () => number): number {
  const date = new Date(t)
  const hour = date.getUTCHours() + date.getUTCMinutes() / 60
  const weekend = [0, 6].includes(date.getUTCDay()) ? 0.35 : 1
  const daytime = Math.max(0, Math.sin((Math.PI * (hour - 7)) / 14)) ** 1.5
  const growth = 1 + ((t - (Date.now() - HISTORY_DAYS * DAY_MS)) / (HISTORY_DAYS * DAY_MS)) * 0.6
  const burst = Math.exp((rand() - 0.5) * 0.9)
  const base = (3 + 38 * daytime) * weekend * growth * burst
  // 深夜一半的桶完全没有流量，网格里出现空档。
  if (daytime < 0.05 && rand() < 0.5) return 0
  return Math.round(base)
}

function bucketFacts(t: number): BucketFacts | null {
  if (cache.has(t)) return cache.get(t) ?? null
  const rand = rng(t / BASE_MS)
  const requests = traffic(t, rand)
  if (requests === 0) {
    cache.set(t, null)
    return null
  }
  const hour = new Date(t).getUTCHours()
  const unknownTokens = rand() < 0.015
  const perProvider = split(
    requests,
    PROVIDERS.map((p) => p.weight),
    rand,
  )
  const global = emptyAcc()
  const providers = PROVIDERS.map((provider, index) => {
    const acc = emptyAcc()
    const finalRequests = perProvider[index]
    if (finalRequests === 0) return acc
    const spiking =
      provider.spike != null &&
      hour >= provider.spike.startHour &&
      hour < provider.spike.startHour + provider.spike.hours
    const errorRate = provider.errorRate + (spiking ? provider.spike!.rate : 0)
    const errors = Math.round(finalRequests * errorRate * (0.6 + 0.8 * rand()))
    const recovered = Math.round(errors * 0.6)
    const failed = Math.min(finalRequests, errors - recovered)
    const successes = finalRequests - failed
    acc.requests = finalRequests
    acc.attempts = finalRequests + recovered
    acc.errors = recovered + failed
    acc.failed = failed
    acc.recovered = recovered
    acc.successes = successes
    acc.durationSum = successes * provider.durationMs * (0.8 + 0.4 * rand())
    acc.firstTokenSum = successes * provider.firstTokenMs * (0.8 + 0.4 * rand())
    const jitter = () => 0.6 + 0.8 * rand()
    if (!unknownTokens) {
      acc.input = Math.round(successes * provider.tokens.input * jitter())
      acc.output = Math.round(successes * provider.tokens.output * jitter())
      acc.cacheRead =
        provider.tokens.cacheRead == null ? null : Math.round(successes * provider.tokens.cacheRead * jitter())
      acc.cacheWrite =
        provider.tokens.cacheWrite == null ? null : Math.round(successes * provider.tokens.cacheWrite * jitter())
      acc.reasoning = null
      acc.tpsTokens = acc.output
      acc.tpsSeconds = acc.output / (provider.tps * (0.85 + 0.3 * rand()))
    }
    if (provider.basis !== 'none') {
      const unpriced = Math.round(acc.attempts * (provider.unpricedShare ?? 0))
      acc.knownAttempts = acc.attempts - unpriced
      acc.unknownAttempts = unpriced
      if (!unknownTokens) {
        const rate = provider.rate
        acc.cost =
          (((acc.input ?? 0) * rate.input +
            (acc.output ?? 0) * rate.output +
            (acc.cacheRead ?? 0) * rate.cacheRead +
            (acc.cacheWrite ?? 0) * rate.cacheWrite) /
            1_000_000) *
          (1 - (provider.unpricedShare ?? 0))
      }
    }
    return acc
  })
  for (const acc of providers) {
    global.attempts += acc.attempts
    global.errors += acc.errors
    global.failed += acc.failed
    global.recovered += acc.recovered
    global.successes += acc.successes
    global.durationSum += acc.durationSum
    global.firstTokenSum += acc.firstTokenSum
    global.tpsTokens += acc.tpsTokens
    global.tpsSeconds += acc.tpsSeconds
    global.cost += acc.cost
    global.knownAttempts += acc.knownAttempts
    global.unknownAttempts += acc.unknownAttempts
    global.input = addNullable(global.input, acc.input)
    global.output = addNullable(global.output, acc.output)
    global.cacheRead = addNullable(global.cacheRead, acc.cacheRead)
    global.cacheWrite = addNullable(global.cacheWrite, acc.cacheWrite)
  }
  global.requests = requests
  global.terminal = requests - Math.round(requests * 0.01)
  global.failed = Math.min(global.failed, global.terminal)
  // 偶尔有一部分请求的恢复判定缺失，用来覆盖“恢复统计不完整”。
  global.recoveryKnown = rand() < 0.03 ? Math.floor(global.terminal * 0.9) : global.terminal

  // API Key / 模型只有请求数独立拆分，Token、成本和延迟按各自权重分摊全局值。
  const share = (weights: readonly number[], counts: number[]) => {
    const total = counts.reduce((sum, count, i) => sum + count * weights[i], 0)
    return counts.map((count, i) => (total > 0 ? (count * weights[i]) / total : 0))
  }
  const scale = (fraction: number, count: number): Acc => {
    const acc = emptyAcc()
    acc.requests = count
    acc.terminal = Math.round(global.terminal * (count / requests))
    acc.successes = Math.round(global.successes * fraction)
    acc.durationSum = global.durationSum * fraction
    acc.firstTokenSum = global.firstTokenSum * fraction
    acc.tpsTokens = global.tpsTokens * fraction
    acc.tpsSeconds = global.tpsSeconds * fraction
    acc.cost = global.cost * fraction
    acc.knownAttempts = Math.round(global.knownAttempts * fraction)
    acc.unknownAttempts = Math.round(global.unknownAttempts * fraction)
    const part = (value: Nullable) => (value == null ? null : Math.round(value * fraction))
    acc.input = part(global.input)
    acc.output = part(global.output)
    acc.cacheRead = part(global.cacheRead)
    acc.cacheWrite = part(global.cacheWrite)
    return acc
  }
  const keyCounts = split(
    requests,
    API_KEYS.map((k) => k.weight),
    rand,
  )
  const keyShares = share(
    API_KEYS.map((k) => k.tokenFactor),
    keyCounts,
  )
  const keys = API_KEYS.map((_, i) => scale(keyShares[i], keyCounts[i]))
  // 模型占比随时间缓慢轮换，悬停不同时段能看到不同的主力模型。
  const modelWeights = MODELS.map(
    (m) => m.weight * (1 + 0.8 * Math.sin((2 * Math.PI * (t / 3_600_000)) / m.periodHours)),
  )
  const modelCounts = split(requests, modelWeights, rand)
  const modelShares = share(
    MODELS.map(() => 1),
    modelCounts,
  )
  const models = MODELS.map((_, i) => scale(modelShares[i], modelCounts[i]))
  const facts = { global, providers, keys, models }
  cache.set(t, facts)
  return facts
}

// ---------------------------------------------------------------- 窗口与筛选

class QueryError extends Error {}

const nowBucket = () => Math.floor(Date.now() / BASE_MS) * BASE_MS
const historyStart = () => Math.floor((Date.now() - HISTORY_DAYS * DAY_MS) / BASE_MS) * BASE_MS

interface Window {
  start: number
  end: number
  explicit: boolean
}

function parseWindow(params: URLSearchParams): Window {
  const rawStart = params.get('start_at')
  const rawEnd = params.get('end_at')
  if ((rawStart == null) !== (rawEnd == null)) throw new QueryError('start_at and end_at must be provided together')
  if (rawStart == null || rawEnd == null) return { start: historyStart(), end: nowBucket() + BASE_MS, explicit: false }
  const start = Number(rawStart)
  const end = Number(rawEnd)
  if (!Number.isInteger(start) || !Number.isInteger(end) || start <= 0 || end <= 0)
    throw new QueryError('window boundaries must be positive integers')
  if (start % BASE_MS !== 0 || end % BASE_MS !== 0) throw new QueryError('window boundaries must align to 900000 ms')
  if (end <= start) throw new QueryError('end_at must be later than start_at')
  return { start, end, explicit: true }
}

type Filter = { kind: 'all' } | { kind: 'provider'; ids: Set<string> } | { kind: 'key'; ids: Set<string> }

function parseFilter(params: URLSearchParams): Filter {
  const providers = params.get('provider_ids')
  const keys = params.get('api_key_ids')
  if (providers != null && keys != null) throw new QueryError('provider_ids and api_key_ids cannot be combined')
  const raw = providers ?? keys
  if (raw == null) return { kind: 'all' }
  const ids = raw.split(',')
  if (raw === '' || ids.some((id) => id === '')) throw new QueryError('stats filter ids must not be empty')
  if (ids.length > MAX_FILTER_IDS) throw new QueryError('stats filter accepts at most 100 ids')
  return providers != null ? { kind: 'provider', ids: new Set(ids) } : { kind: 'key', ids: new Set(ids) }
}

function rejectUnknown(params: URLSearchParams, allowed: readonly string[]): void {
  for (const name of params.keys()) if (!allowed.includes(name)) throw new QueryError(`unknown query parameter ${name}`)
}

/** 所选范围内、按筛选口径折算后的单桶累计。 */
function scoped(facts: BucketFacts, filter: Filter): Acc {
  if (filter.kind === 'all') return facts.global
  const acc = emptyAcc()
  if (filter.kind === 'provider') {
    PROVIDERS.forEach((provider, i) => {
      if (provider.id == null || !filter.ids.has(provider.id)) return
      const part = facts.providers[i]
      merge(acc, part)
    })
    // 供应商口径：请求数就是上游尝试数，请求级分类列在边际桶里不存在。
    acc.requests = acc.attempts
    acc.terminal = 0
    acc.failed = 0
    acc.recovered = 0
    acc.recoveryKnown = 0
    return acc
  }
  API_KEYS.forEach((key, i) => {
    if (key.id === '' || !filter.ids.has(key.id)) return
    merge(acc, facts.keys[i])
  })
  acc.attempts = 0
  acc.errors = 0
  acc.terminal = 0
  acc.failed = 0
  acc.recovered = 0
  acc.recoveryKnown = 0
  return acc
}

/** 订阅供应商每毫秒的摊销（美元）；API Key 口径没有订阅。 */
function amortizationPerMs(filter: Filter): number {
  if (filter.kind === 'key') return 0
  return PROVIDERS.reduce((sum, provider) => {
    if (provider.basis !== 'subscription' || provider.monthlyPrice == null) return sum
    if (filter.kind === 'provider' && (provider.id == null || !filter.ids.has(provider.id))) return sum
    return sum + provider.monthlyPrice / (30 * DAY_MS)
  }, 0)
}

// ---------------------------------------------------------------- 序列化

const money = (usd: number, known: number, unknown: number) => ({
  amount: usd.toFixed(6),
  known_count: known,
  unknown_count: unknown,
})
const avg = (sum: number, count: number): number | null => (count > 0 ? Math.round(sum / count) : null)
const tpsOf = (acc: Acc): number | null =>
  acc.tpsSeconds > 0 ? Math.round((acc.tpsTokens / acc.tpsSeconds) * 10) / 10 : null

function metrics(acc: Acc, amortizedUsd: number) {
  const priced = acc.knownAttempts + acc.unknownAttempts > 0
  const entirelyUnknown = acc.knownAttempts === 0 && acc.unknownAttempts > 0
  return {
    total_input_tokens: acc.input,
    total_output_tokens: acc.output,
    total_cache_read_tokens: acc.cacheRead,
    total_cache_write_tokens: acc.cacheWrite,
    total_reasoning_tokens: acc.reasoning,
    avg_duration_ms: avg(acc.durationSum, acc.successes),
    avg_first_token_ms: avg(acc.firstTokenSum, acc.successes),
    avg_output_tps: tpsOf(acc),
    currency: USD,
    actual_cost:
      priced || amortizedUsd > 0
        ? money(entirelyUnknown ? amortizedUsd : acc.cost + amortizedUsd, acc.knownAttempts, acc.unknownAttempts)
        : money(0, 0, 0),
  }
}

function overview(window: Window, filter: Filter) {
  const total = emptyAcc()
  let seen = false
  for (let t = window.start; t < window.end; t += BASE_MS) {
    const facts = bucketFacts(t)
    if (facts == null) continue
    seen = true
    merge(total, scoped(facts, filter))
  }
  const spanMs = window.end - window.start
  const amortized = amortizationPerMs(filter) * spanMs
  const subscription =
    filter.kind === 'key' || amortized === 0
      ? null
      : {
          cost: money(amortized, 0, 0),
          usage_equivalent_cost: money(
            PROVIDERS.reduce((sum, p, i) => {
              if (p.basis !== 'subscription') return sum
              if (filter.kind === 'provider' && (p.id == null || !filter.ids.has(p.id))) return sum
              return sum + providerCostInWindow(window, i)
            }, 0),
            0,
            0,
          ),
          multiple: null as string | null,
        }
  if (subscription) {
    const equivalent = Number(subscription.usage_equivalent_cost.amount)
    subscription.multiple = amortized > 0 ? (equivalent / amortized).toFixed(4) : null
  }
  const base = metrics(total, seen || amortized > 0 ? amortized : 0)
  return {
    total_requests: total.requests,
    terminal_request_count: total.terminal,
    failed_request_count: total.failed,
    recovered_request_count: total.recovered,
    recovery_known_request_count: total.recoveryKnown,
    upstream_request_count: total.attempts,
    upstream_error_count: total.errors,
    ...base,
    subscription,
  }
}

function providerCostInWindow(window: Window, index: number): number {
  let cost = 0
  for (let t = window.start; t < window.end; t += BASE_MS) cost += bucketFacts(t)?.providers[index].cost ?? 0
  return cost
}

function series(window: Window, filter: Filter, requestedBucketMs: number) {
  const span = window.end - window.start
  const minimum = Math.ceil(span / MAX_BUCKETS / BASE_MS) * BASE_MS
  const bucketMs = Math.max(requestedBucketMs, minimum)
  const rows = new Map<number, Acc>()
  for (let t = window.start; t < window.end; t += BASE_MS) {
    const facts = bucketFacts(t)
    if (facts == null) continue
    const at = window.start + Math.floor((t - window.start) / bucketMs) * bucketMs
    const acc = rows.get(at) ?? emptyAcc()
    merge(acc, scoped(facts, filter))
    rows.set(at, acc)
  }
  const perMs = amortizationPerMs(filter)
  return [...rows.entries()]
    .sort(([a], [b]) => a - b)
    .map(([at, acc]) => ({
      bucket_ms: bucketMs,
      bucket_start: at,
      request_count: acc.requests,
      terminal_request_count: acc.terminal,
      failed_request_count: acc.failed,
      recovered_request_count: acc.recovered,
      recovery_known_request_count: acc.recoveryKnown,
      upstream_request_count: acc.attempts,
      upstream_error_count: acc.errors,
      ...metrics(acc, perMs * bucketMs),
    }))
    .filter((row) => row.request_count > 0 || row.upstream_request_count > 0)
}

function collect(window: Window, pick: (facts: BucketFacts) => Acc[], count: number) {
  const accs = Array.from({ length: count }, emptyAcc)
  const lastSeen = Array<number>(count).fill(0)
  for (let t = window.start; t < window.end; t += BASE_MS) {
    const facts = bucketFacts(t)
    if (facts == null) continue
    pick(facts).forEach((part, i) => {
      merge(accs[i], part)
      if (part.requests > 0) lastSeen[i] = t
    })
  }
  return { accs, lastSeen }
}

function models(window: Window) {
  const { accs } = collect(window, (facts) => facts.models, MODELS.length)
  return MODELS.flatMap((model, i) => {
    const acc = accs[i]
    if (acc.requests === 0) return []
    return [
      {
        model_id: model.id,
        deleted: model.deleted ?? false,
        model: model.name,
        request_count: acc.requests,
        total_input_tokens: acc.input,
        total_output_tokens: acc.output,
        total_reasoning_tokens: acc.reasoning,
        avg_duration_ms: avg(acc.durationSum, acc.successes),
        currency: USD,
        cost: money(acc.cost, acc.knownAttempts, acc.unknownAttempts),
        billing: money(0, 0, 0),
      },
    ]
  }).sort((a, b) => b.request_count - a.request_count)
}

function providers(window: Window) {
  const { accs } = collect(window, (facts) => facts.providers, PROVIDERS.length)
  const days = (window.end - window.start) / DAY_MS
  return PROVIDERS.flatMap((provider, i) => {
    const acc = accs[i]
    if (provider.deleted && acc.attempts === 0) return []
    const amortized =
      provider.basis === 'subscription' && provider.monthlyPrice != null ? (provider.monthlyPrice / 30) * days : 0
    const cost = provider.basis === 'none' ? null : money(acc.cost, acc.knownAttempts, acc.unknownAttempts)
    const subscription =
      amortized > 0
        ? {
            cost: money(amortized, 0, 0),
            usage_equivalent_cost: money(acc.cost, 0, 0),
            multiple: (acc.cost / amortized).toFixed(4),
          }
        : null
    return [
      {
        provider_id: provider.id,
        deleted: provider.deleted ?? false,
        provider: provider.name,
        upstream_request_count: acc.attempts,
        upstream_error_count: acc.errors,
        avg_duration_ms: avg(acc.durationSum, acc.successes),
        avg_output_tps: tpsOf(acc),
        currency: USD,
        cost,
        subscription,
        allocated_cost:
          provider.basis === 'none'
            ? null
            : money(
                (provider.basis === 'subscription' ? 0 : acc.cost) + amortized,
                acc.knownAttempts,
                acc.unknownAttempts,
              ),
      },
    ]
  }).sort((a, b) => b.upstream_request_count - a.upstream_request_count)
}

function apiKeys(window: Window) {
  const { accs, lastSeen } = collect(window, (facts) => facts.keys, API_KEYS.length)
  return API_KEYS.flatMap((key, i) => {
    const acc = accs[i]
    if (key.deleted && acc.requests === 0) return []
    return [
      {
        deleted: key.deleted ?? false,
        api_key_id: key.id,
        api_key_name: key.name,
        request_count: acc.requests,
        total_input_tokens: acc.input,
        total_output_tokens: acc.output,
        cache_read_tokens: acc.cacheRead,
        cache_write_tokens: acc.cacheWrite,
        reasoning_tokens: acc.reasoning,
        last_used_at: lastSeen[i],
        currency: USD,
        cost: money(acc.cost, acc.knownAttempts, acc.unknownAttempts),
        billing: money(0, 0, 0),
      },
    ]
  }).sort((a, b) => b.request_count - a.request_count)
}

// ---------------------------------------------------------------- HTTP

interface LoggedRequest {
  at: number
  path: string
  query: Record<string, string>
  status: number
}
const requestLog: LoggedRequest[] = []

const json = (body: unknown, status = 200) => Response.json(body, { status })

async function handleStats(path: string, params: URLSearchParams): Promise<Response> {
  if (config.delayMs > 0) await Bun.sleep(config.delayMs)
  if (config.failStats) return json({ error: 'Usage statistics could not be loaded', code: 'stats_query_failed' }, 500)
  try {
    if (path === '/stats/overview') {
      rejectUnknown(params, ['start_at', 'end_at', 'provider_ids', 'api_key_ids'])
      return json({ data: overview(parseWindow(params), parseFilter(params)) })
    }
    if (path === '/stats/series') {
      rejectUnknown(params, ['start_at', 'end_at', 'bucket', 'provider_ids', 'api_key_ids'])
      const window = parseWindow(params)
      const filter = parseFilter(params)
      const bucket = Number(params.get('bucket') ?? BASE_MS / 1000)
      if (!Number.isInteger(bucket) || bucket < 900 || bucket % 900 !== 0)
        throw new QueryError('bucket must be a positive multiple of 900 seconds')
      return json({ data: series(window, filter, bucket * 1000) })
    }
    if (path === '/stats/models') return json({ data: models(parseWindow(params)) })
    if (path === '/stats/providers') return json({ data: providers(parseWindow(params)) })
    if (path === '/stats/api-keys') return json({ data: apiKeys(parseWindow(params)) })
  } catch (error) {
    if (error instanceof QueryError) return json({ error: error.message, code: 'invalid_stats_query' }, 400)
    throw error
  }
  return json({ error: 'not found', code: 'not_found' }, 404)
}

// 页面外壳（登录态、版本、设置）需要的最小应答；其余未知接口一律 404 并打印，防止悄悄缺数据。
function handleShell(path: string, method: string): Response | null {
  if (path === '/startup') return json({ status: 'ready', progress: null })
  if (path === '/auth/state')
    return json({ mode: 'server', authenticated: true, setup_authorized: false, username: 'mock-admin' })
  if (path === '/status') return json({ data: { status: 'running' } })
  if (path === '/updates' || path === '/updates/check') {
    return json({
      data: {
        current_version: '0.6.1',
        check_status: 'up-to-date',
        last_success_at: '2026-11-02T00:00:00Z',
        last_failure: null,
        available_update: null,
        skipped: false,
        download_supported: false,
      },
    })
  }
  if (path.startsWith('/settings/')) return json({ data: method === 'GET' ? '' : null })
  if (path === '/currency') return json({ data: { custom: null } })
  // Chat 侧栏会列出密钥与模型路由；用量页不依赖它们，返回空列表即可。
  if (path === '/api-keys' || path === '/models' || path === '/providers') return json({ data: [] })
  return null
}

Bun.serve({
  hostname: host,
  port,
  async fetch(request) {
    const url = new URL(request.url)
    const pathname = decodeURIComponent(url.pathname)

    if (pathname === '/__mock/config') {
      if (request.method === 'POST') Object.assign(config, await request.json())
      return json(config)
    }
    if (pathname === '/__mock/requests') {
      if (request.method === 'DELETE') requestLog.length = 0
      return json(requestLog)
    }

    if (pathname.startsWith('/api/v1/')) {
      const path = pathname.slice('/api/v1'.length)
      if (path.startsWith('/stats/')) {
        const response = await handleStats(path, url.searchParams)
        requestLog.push({ at: Date.now(), path, query: Object.fromEntries(url.searchParams), status: response.status })
        return response
      }
      const shell = handleShell(path, request.method)
      if (shell) return shell
      console.warn(`[mock] 未实现的接口 ${request.method} ${pathname}`)
      return json({ error: 'not implemented in mock', code: 'mock_not_implemented' }, 404)
    }
    if (pathname === '/api' || pathname.startsWith('/api/') || pathname.startsWith('/v1')) {
      return new Response('not served by the mock', { status: 404 })
    }

    const filePath = resolve(dist, `.${pathname}`)
    if (filePath.startsWith(dist + sep)) {
      const info = await stat(filePath).catch(() => null)
      if (info?.isFile()) return new Response(Bun.file(filePath))
    }
    return new Response(Bun.file(indexPath))
  },
})

console.log(`mock-stats-server listening on http://${host}:${port} (delay ${config.delayMs}ms, seed ${seed})`)
