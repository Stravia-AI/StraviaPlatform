import { describe, expect, test } from 'bun:test'

import { providerCredentialFieldLabel } from '../src/lib/provider-credential-labels'
import type { VendorCredentialField } from '../src/lib/types'

const EN = 'en-US' as const
const ZH = 'zh-CN' as const

function field(key: string, fallback = 'Backend fallback'): VendorCredentialField {
  return { key, label: fallback, secret: false, required: false, input: 'text' }
}

describe('provider credential labels', () => {
  test('localizes every built-in dynamic credential field', () => {
    const labels = [
      ['amazon-bedrock', 'region', 'AWS Region', 'AWS 区域'],
      ['amazon-bedrock', 'apiKey', 'Bedrock API Key', 'Bedrock API 密钥'],
      ['amazon-bedrock', 'accessKeyId', 'Access Key ID', '访问密钥 ID'],
      ['amazon-bedrock', 'secretAccessKey', 'Secret Access Key', '访问密钥'],
      ['amazon-bedrock', 'sessionToken', 'Session Token', '会话令牌'],
      ['azure', 'resourceName', 'Azure Resource Name', 'Azure 资源名称'],
      ['azure', 'apiKey', 'API Key', 'API 密钥'],
      ['azure', 'apiVersion', 'API Version', 'API 版本'],
      ['cloudflare-ai-gateway', 'apiToken', 'AI Gateway API Token', 'AI Gateway API 令牌'],
      ['cloudflare-ai-gateway', 'accountId', 'Cloudflare Account ID', 'Cloudflare 账户 ID'],
      ['cloudflare-ai-gateway', 'gatewayId', 'AI Gateway ID', 'AI Gateway ID'],
      ['gitlab', 'apiKey', 'GitLab Access Token', 'GitLab 访问令牌'],
      ['gitlab', 'instanceUrl', 'GitLab Instance URL', 'GitLab 实例 URL'],
      ['gitlab', 'aiGatewayUrl', 'GitLab AI Gateway URL', 'GitLab AI Gateway URL'],
      ['google-vertex', 'project', 'Google Cloud Project', 'Google Cloud 项目'],
      ['google-vertex', 'location', 'Google Cloud Location', 'Google Cloud 区域'],
      ['google-vertex', 'credentials', 'Service Account JSON', '服务账号 JSON'],
      ['google-vertex', 'apiKey', 'Vertex API Key', 'Vertex API 密钥'],
      ['google-vertex-anthropic', 'project', 'Google Cloud Project', 'Google Cloud 项目'],
      ['google-vertex-anthropic', 'location', 'Google Cloud Location', 'Google Cloud 区域'],
      ['google-vertex-anthropic', 'credentials', 'Service Account JSON', '服务账号 JSON'],
      ['google-vertex-anthropic', 'apiKey', 'Vertex API Key', 'Vertex API 密钥'],
      ['openrouter', 'apiKey', 'API Key', 'API 密钥'],
      ['openrouter', 'httpReferer', 'App Referer URL', '应用来源 URL'],
      ['openrouter', 'xTitle', 'App Title', '应用名称'],
      ['sap-ai-core', 'deploymentUrl', 'Deployment URL', '部署 URL'],
      ['sap-ai-core', 'tokenUrl', 'OAuth Token URL', 'OAuth 令牌 URL'],
      ['sap-ai-core', 'clientId', 'OAuth Client ID', 'OAuth 客户端 ID'],
      ['sap-ai-core', 'clientSecret', 'OAuth Client Secret', 'OAuth 客户端密钥'],
      ['sap-ai-core', 'resourceGroup', 'Resource Group', '资源组'],
      ['watsonx', 'apiKey', 'IBM Cloud API Key', 'IBM Cloud API 密钥'],
      ['watsonx', 'projectId', 'Project ID', '项目 ID'],
      ['watsonx', 'baseUrl', 'Service URL', '服务 URL'],
      ['watsonx', 'apiVersion', 'API Version', 'API 版本'],
      ['cohere', 'apiKey', 'API Key', 'API 密钥'],
    ] as const

    for (const [vendorId, key, english, chinese] of labels) {
      expect(providerCredentialFieldLabel(vendorId, field(key), EN)).toBe(english)
      expect(providerCredentialFieldLabel(vendorId, field(key), ZH)).toBe(chinese)
    }
  })

  test('falls back to the canonical backend English label for unknown fields', () => {
    expect(providerCredentialFieldLabel('future-vendor', field('futureField', 'Future field'), ZH)).toBe('Future field')
  })
})
