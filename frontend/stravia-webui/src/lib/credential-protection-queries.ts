import { admin } from '#lib/admin-client.js'

export const CUSTOM_CREDENTIAL_RULES_KEY = ['credential-protection-custom-rules']

/** 凭据保护页与请求记录里的命中高亮共用同一份缓存。 */
export const credentialRulesQuery = (enabled = true) => ({
  queryKey: ['credential-protection-rules'],
  queryFn: admin.credentialProtection.rules,
  enabled,
})

export const customCredentialRulesQuery = (enabled = true) => ({
  queryKey: CUSTOM_CREDENTIAL_RULES_KEY,
  queryFn: admin.credentialProtection.customRules.list,
  enabled,
})

/**
 * 命中详情只在侧栏打开且选中了某条命中时请求；键里带记录 ID 与命中下标，切换时不会沿用上一条的数据。
 * 404 表示该条目已过期或下标不存在，重试没有意义，交由界面直接说明。
 */
export const credentialHitDetailQuery = (recordId: string | null, index: number, enabled: boolean) => ({
  queryKey: ['credential-protection-hit-detail', recordId, index],
  queryFn: () => {
    if (recordId === null) throw new Error('Hit detail requested without a record')
    return admin.credentialProtection.hit(recordId, index)
  },
  enabled: enabled && recordId !== null,
  retry: false,
})

/** 后续复用与当前选中的命中下标无关，整行侧栏打开时取一次即可。 */
export const credentialHitReusesQuery = (recordId: string | null, enabled: boolean) => ({
  queryKey: ['credential-protection-hit-reuses', recordId],
  queryFn: () => {
    if (recordId === null) throw new Error('Reuse history requested without a record')
    return admin.credentialProtection.reuses(recordId)
  },
  enabled: enabled && recordId !== null,
  retry: false,
})
