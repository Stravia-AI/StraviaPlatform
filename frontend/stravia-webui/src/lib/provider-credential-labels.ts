import * as m from '$lib/paraglide/messages.js'
import type { Locale } from '$lib/localization.svelte'
import type { VendorCredentialField } from '$lib/types'

type CredentialLabel = (locale: Locale) => string

const GENERIC_CREDENTIAL_LABELS: Record<string, CredentialLabel> = {
  apiKey: (locale) => m.common_api_key({}, { locale }),
  apiVersion: (locale) => m.provider_credential_api_version({}, { locale }),
}

const VENDOR_CREDENTIAL_LABELS: Record<string, Record<string, CredentialLabel>> = {
  'amazon-bedrock': {
    region: (locale) => m.provider_credential_aws_region({}, { locale }),
    apiKey: (locale) => m.provider_credential_bedrock_api_key({}, { locale }),
    accessKeyId: (locale) => m.provider_credential_access_key_id({}, { locale }),
    secretAccessKey: (locale) => m.provider_credential_secret_access_key({}, { locale }),
    sessionToken: (locale) => m.provider_credential_session_token({}, { locale }),
  },
  azure: { resourceName: (locale) => m.provider_credential_azure_resource_name({}, { locale }) },
  'cloudflare-ai-gateway': {
    apiToken: (locale) => m.provider_credential_ai_gateway_api_token({}, { locale }),
    accountId: (locale) => m.provider_credential_cloudflare_account_id({}, { locale }),
    gatewayId: (locale) => m.provider_credential_ai_gateway_id({}, { locale }),
  },
  gitlab: {
    apiKey: (locale) => m.provider_credential_gitlab_access_token({}, { locale }),
    instanceUrl: (locale) => m.provider_credential_gitlab_instance_url({}, { locale }),
    aiGatewayUrl: (locale) => m.provider_credential_gitlab_ai_gateway_url({}, { locale }),
  },
  'google-vertex': {
    project: (locale) => m.provider_credential_google_cloud_project({}, { locale }),
    location: (locale) => m.provider_credential_google_cloud_location({}, { locale }),
    credentials: (locale) => m.provider_credential_service_account_json({}, { locale }),
    apiKey: (locale) => m.provider_credential_vertex_api_key({}, { locale }),
  },
  'google-vertex-anthropic': {
    project: (locale) => m.provider_credential_google_cloud_project({}, { locale }),
    location: (locale) => m.provider_credential_google_cloud_location({}, { locale }),
    credentials: (locale) => m.provider_credential_service_account_json({}, { locale }),
    apiKey: (locale) => m.provider_credential_vertex_api_key({}, { locale }),
  },
  openrouter: {
    httpReferer: (locale) => m.provider_credential_app_referer_url({}, { locale }),
    xTitle: (locale) => m.provider_credential_app_title({}, { locale }),
  },
  'sap-ai-core': {
    deploymentUrl: (locale) => m.provider_credential_deployment_url({}, { locale }),
    tokenUrl: (locale) => m.provider_credential_oauth_token_url({}, { locale }),
    clientId: (locale) => m.provider_credential_oauth_client_id({}, { locale }),
    clientSecret: (locale) => m.provider_credential_oauth_client_secret({}, { locale }),
    resourceGroup: (locale) => m.provider_credential_resource_group({}, { locale }),
  },
  watsonx: {
    apiKey: (locale) => m.provider_credential_ibm_cloud_api_key({}, { locale }),
    projectId: (locale) => m.provider_credential_project_id({}, { locale }),
    baseUrl: (locale) => m.provider_credential_service_url({}, { locale }),
  },
}

/**
 * Resolves every built-in Vendor credential label through the WebUI catalog.
 * Backend metadata remains the English fallback for a newer unknown field so
 * an older WebUI never selects a misleading translation.
 */
export function providerCredentialFieldLabel(vendorId: string, field: VendorCredentialField, locale: Locale): string {
  const label = VENDOR_CREDENTIAL_LABELS[vendorId]?.[field.key] ?? GENERIC_CREDENTIAL_LABELS[field.key]
  return label ? label(locale) : field.label
}
