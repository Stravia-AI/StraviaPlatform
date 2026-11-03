import { admin } from '#lib/admin-client.js'
import { ObservationContents } from '#lib/observation-contents.js'

/** 页面共享的正文读取与缓存；卡片预览与 Tooltip 全文共用同一份。 */
export const observationContents = new ObservationContents(admin.observations.contents)
