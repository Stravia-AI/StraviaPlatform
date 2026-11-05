import { infiniteQueryOptions } from '@tanstack/svelte-query'

import { admin } from '#lib/admin-client.js'
import { HIT_REUSE_PAGE_SIZE } from '#lib/credential-hit-records.js'

export const CUSTOM_CREDENTIAL_RULES_KEY = ['credential-protection-custom-rules']
export const CREDENTIAL_RULES_KEY = ['credential-protection-rules']

/** 凭据保护页与请求记录里的命中高亮共用同一份缓存。 */
export const credentialRulesQuery = (enabled = true) => ({
  queryKey: CREDENTIAL_RULES_KEY,
  queryFn: admin.credentialProtection.rules,
  enabled,
})

export const customCredentialRulesQuery = (enabled = true) => ({
  queryKey: CUSTOM_CREDENTIAL_RULES_KEY,
  queryFn: admin.credentialProtection.customRules.list,
  enabled,
})

/**
 * 命中详情只在侧栏打开且选中了某条命中时请求；键里带记录 ID、命中下标与明文变体，切换时不会沿用上一条的数据。
 * 遮罩与明文分开缓存：隐藏明文后立刻回到已缓存的遮罩结果，不重新请求。
 * 404 表示该条目已过期或下标不存在，重试没有意义，交由界面直接说明。
 */
export const credentialHitDetailQuery = (recordId: string | null, index: number, enabled: boolean, reveal = false) => ({
  queryKey: ['credential-protection-hit-detail', recordId, index, reveal ? 'revealed' : 'masked'],
  queryFn: () => {
    if (recordId === null) throw new Error('Hit detail requested without a record')
    return admin.credentialProtection.hit(recordId, index, { reveal })
  },
  enabled: enabled && recordId !== null,
  retry: false,
})

/** 后续复用与当前选中的命中下标无关；首次展开时才取第一页，之后按游标追加并保留已加载页。 */
export const credentialHitReusesQuery = (recordId: string | null, enabled: boolean) =>
  infiniteQueryOptions({
    queryKey: ['credential-protection-hit-reuses', recordId],
    queryFn: ({ pageParam }) => {
      if (recordId === null) throw new Error('Reuse history requested without a record')
      return admin.credentialProtection.reuses(recordId, { cursor: pageParam, limit: HIT_REUSE_PAGE_SIZE })
    },
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
    enabled: enabled && recordId !== null,
    retry: false,
  })
