-- Legacy adapters stored all declared fields in adapter_credentials, including
-- ordinary connection settings. Move only the fields that are non-secret in the
-- current provider descriptors. Existing vendor_options values take precedence.
UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.resourceName') IS NULL THEN json_set(vendor_options, '$.resourceName', json_extract(adapter_credentials, '$.resourceName')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.resourceName')
WHERE vendor = 'azure' AND json_type(adapter_credentials, '$.resourceName') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.apiVersion') IS NULL THEN json_set(vendor_options, '$.apiVersion', json_extract(adapter_credentials, '$.apiVersion')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.apiVersion')
WHERE vendor IN ('azure', 'watsonx') AND json_type(adapter_credentials, '$.apiVersion') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.region') IS NULL THEN json_set(vendor_options, '$.region', json_extract(adapter_credentials, '$.region')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.region')
WHERE vendor = 'amazon-bedrock' AND json_type(adapter_credentials, '$.region') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.project') IS NULL THEN json_set(vendor_options, '$.project', json_extract(adapter_credentials, '$.project')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.project')
WHERE vendor IN ('google-vertex', 'google-vertex-anthropic') AND json_type(adapter_credentials, '$.project') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.location') IS NULL THEN json_set(vendor_options, '$.location', json_extract(adapter_credentials, '$.location')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.location')
WHERE vendor IN ('google-vertex', 'google-vertex-anthropic') AND json_type(adapter_credentials, '$.location') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.deploymentUrl') IS NULL THEN json_set(vendor_options, '$.deploymentUrl', json_extract(adapter_credentials, '$.deploymentUrl')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.deploymentUrl')
WHERE vendor = 'sap-ai-core' AND json_type(adapter_credentials, '$.deploymentUrl') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.tokenUrl') IS NULL THEN json_set(vendor_options, '$.tokenUrl', json_extract(adapter_credentials, '$.tokenUrl')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.tokenUrl')
WHERE vendor = 'sap-ai-core' AND json_type(adapter_credentials, '$.tokenUrl') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.resourceGroup') IS NULL THEN json_set(vendor_options, '$.resourceGroup', json_extract(adapter_credentials, '$.resourceGroup')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.resourceGroup')
WHERE vendor = 'sap-ai-core' AND json_type(adapter_credentials, '$.resourceGroup') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.instanceUrl') IS NULL THEN json_set(vendor_options, '$.instanceUrl', json_extract(adapter_credentials, '$.instanceUrl')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.instanceUrl')
WHERE vendor = 'gitlab' AND json_type(adapter_credentials, '$.instanceUrl') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.aiGatewayUrl') IS NULL THEN json_set(vendor_options, '$.aiGatewayUrl', json_extract(adapter_credentials, '$.aiGatewayUrl')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.aiGatewayUrl')
WHERE vendor = 'gitlab' AND json_type(adapter_credentials, '$.aiGatewayUrl') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.projectId') IS NULL THEN json_set(vendor_options, '$.projectId', json_extract(adapter_credentials, '$.projectId')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.projectId')
WHERE vendor = 'watsonx' AND json_type(adapter_credentials, '$.projectId') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.baseUrl') IS NULL THEN json_set(vendor_options, '$.baseUrl', json_extract(adapter_credentials, '$.baseUrl')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.baseUrl')
WHERE vendor = 'watsonx' AND json_type(adapter_credentials, '$.baseUrl') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.accountId') IS NULL THEN json_set(vendor_options, '$.accountId', json_extract(adapter_credentials, '$.accountId')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.accountId')
WHERE vendor = 'cloudflare-ai-gateway' AND json_type(adapter_credentials, '$.accountId') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.gatewayId') IS NULL THEN json_set(vendor_options, '$.gatewayId', json_extract(adapter_credentials, '$.gatewayId')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.gatewayId')
WHERE vendor = 'cloudflare-ai-gateway' AND json_type(adapter_credentials, '$.gatewayId') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.httpReferer') IS NULL THEN json_set(vendor_options, '$.httpReferer', json_extract(adapter_credentials, '$.httpReferer')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.httpReferer')
WHERE vendor = 'openrouter' AND json_type(adapter_credentials, '$.httpReferer') IS NOT NULL;

UPDATE providers
SET vendor_options = CASE WHEN json_type(vendor_options, '$.xTitle') IS NULL THEN json_set(vendor_options, '$.xTitle', json_extract(adapter_credentials, '$.xTitle')) ELSE vendor_options END,
    adapter_credentials = json_remove(adapter_credentials, '$.xTitle')
WHERE vendor = 'openrouter' AND json_type(adapter_credentials, '$.xTitle') IS NOT NULL;
