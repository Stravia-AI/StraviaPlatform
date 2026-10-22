import type { HandleClientError } from '@sveltejs/kit/hooks'

import { classifyRouteError } from '#lib/route-error.js'

// 原始异常只进入浏览器控制台；页面只拿到经过分类的安全原因。
export const handleError: HandleClientError = ({ error }) => {
  console.error(error)
  return { reason: classifyRouteError(error) }
}
