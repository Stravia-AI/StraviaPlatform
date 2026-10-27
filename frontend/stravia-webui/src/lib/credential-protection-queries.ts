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
