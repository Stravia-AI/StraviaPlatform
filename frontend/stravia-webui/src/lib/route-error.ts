/**
 * 全局错误页可以安全展示的失败原因。
 *
 * 路由错误的原始异常可能携带内部地址、凭据或堆栈，因此只识别下面两类浏览器自身的
 * 固定失败形态，并映射为本地化文案；其余一律回退到通用恢复说明，绝不回显异常文本。
 */
export type RouteErrorReason = 'module_load' | 'network'

const moduleLoadFailure = /dynamically imported module|importing a module script failed/i
const networkFailure = /^(failed to fetch|networkerror when attempting to fetch resource\.?|load failed)$/i

export function classifyRouteError(error: unknown): RouteErrorReason | undefined {
  if (!(error instanceof TypeError)) return undefined
  if (moduleLoadFailure.test(error.message)) return 'module_load'
  if (networkFailure.test(error.message.trim())) return 'network'
  return undefined
}
