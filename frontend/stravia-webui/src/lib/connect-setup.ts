import { createContext } from 'svelte'
import type { ClaudeModelMappings, CliToolId, CodeLanguage, GatewayProtocol } from '$lib/connect'

export interface ConnectDraft {
  tab: 'cli' | 'code'
  codeLanguage: CodeLanguage
  codeProtocol: GatewayProtocol
  codeModelId: string
  codeKeyId: string
  cliToolId: CliToolId
  cliKeyId: string
  claudeModelIds: Record<keyof ClaudeModelMappings, string>
}

export interface ConnectSetup {
  draft: ConnectDraft | undefined
  createKey: boolean
}

// 由根布局持有，仅在补齐资源期间保留选择 ID，不保存 secret 或写入浏览器存储。
export const [getConnectSetup, setConnectSetup] = createContext<ConnectSetup>()
