-- Legacy adapters stored all declared fields in adapter_credentials, including
-- ordinary connection settings. Move only the fields that are non-secret in the
-- current provider descriptors. Existing vendor_options values take precedence.
UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'resourceName' THEN jsonb_set(vendor_options::jsonb, '{resourceName}', adapter_credentials::jsonb -> 'resourceName', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'resourceName')::text
WHERE vendor = 'azure' AND adapter_credentials::jsonb ? 'resourceName';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'apiVersion' THEN jsonb_set(vendor_options::jsonb, '{apiVersion}', adapter_credentials::jsonb -> 'apiVersion', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'apiVersion')::text
WHERE vendor IN ('azure', 'watsonx') AND adapter_credentials::jsonb ? 'apiVersion';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'region' THEN jsonb_set(vendor_options::jsonb, '{region}', adapter_credentials::jsonb -> 'region', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'region')::text
WHERE vendor = 'amazon-bedrock' AND adapter_credentials::jsonb ? 'region';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'project' THEN jsonb_set(vendor_options::jsonb, '{project}', adapter_credentials::jsonb -> 'project', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'project')::text
WHERE vendor IN ('google-vertex', 'google-vertex-anthropic') AND adapter_credentials::jsonb ? 'project';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'location' THEN jsonb_set(vendor_options::jsonb, '{location}', adapter_credentials::jsonb -> 'location', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'location')::text
WHERE vendor IN ('google-vertex', 'google-vertex-anthropic') AND adapter_credentials::jsonb ? 'location';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'deploymentUrl' THEN jsonb_set(vendor_options::jsonb, '{deploymentUrl}', adapter_credentials::jsonb -> 'deploymentUrl', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'deploymentUrl')::text
WHERE vendor = 'sap-ai-core' AND adapter_credentials::jsonb ? 'deploymentUrl';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'tokenUrl' THEN jsonb_set(vendor_options::jsonb, '{tokenUrl}', adapter_credentials::jsonb -> 'tokenUrl', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'tokenUrl')::text
WHERE vendor = 'sap-ai-core' AND adapter_credentials::jsonb ? 'tokenUrl';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'resourceGroup' THEN jsonb_set(vendor_options::jsonb, '{resourceGroup}', adapter_credentials::jsonb -> 'resourceGroup', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'resourceGroup')::text
WHERE vendor = 'sap-ai-core' AND adapter_credentials::jsonb ? 'resourceGroup';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'instanceUrl' THEN jsonb_set(vendor_options::jsonb, '{instanceUrl}', adapter_credentials::jsonb -> 'instanceUrl', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'instanceUrl')::text
WHERE vendor = 'gitlab' AND adapter_credentials::jsonb ? 'instanceUrl';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'aiGatewayUrl' THEN jsonb_set(vendor_options::jsonb, '{aiGatewayUrl}', adapter_credentials::jsonb -> 'aiGatewayUrl', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'aiGatewayUrl')::text
WHERE vendor = 'gitlab' AND adapter_credentials::jsonb ? 'aiGatewayUrl';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'projectId' THEN jsonb_set(vendor_options::jsonb, '{projectId}', adapter_credentials::jsonb -> 'projectId', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'projectId')::text
WHERE vendor = 'watsonx' AND adapter_credentials::jsonb ? 'projectId';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'baseUrl' THEN jsonb_set(vendor_options::jsonb, '{baseUrl}', adapter_credentials::jsonb -> 'baseUrl', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'baseUrl')::text
WHERE vendor = 'watsonx' AND adapter_credentials::jsonb ? 'baseUrl';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'accountId' THEN jsonb_set(vendor_options::jsonb, '{accountId}', adapter_credentials::jsonb -> 'accountId', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'accountId')::text
WHERE vendor = 'cloudflare-ai-gateway' AND adapter_credentials::jsonb ? 'accountId';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'gatewayId' THEN jsonb_set(vendor_options::jsonb, '{gatewayId}', adapter_credentials::jsonb -> 'gatewayId', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'gatewayId')::text
WHERE vendor = 'cloudflare-ai-gateway' AND adapter_credentials::jsonb ? 'gatewayId';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'httpReferer' THEN jsonb_set(vendor_options::jsonb, '{httpReferer}', adapter_credentials::jsonb -> 'httpReferer', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'httpReferer')::text
WHERE vendor = 'openrouter' AND adapter_credentials::jsonb ? 'httpReferer';

UPDATE providers
SET vendor_options = CASE WHEN NOT vendor_options::jsonb ? 'xTitle' THEN jsonb_set(vendor_options::jsonb, '{xTitle}', adapter_credentials::jsonb -> 'xTitle', true)::text ELSE vendor_options END,
    adapter_credentials = (adapter_credentials::jsonb - 'xTitle')::text
WHERE vendor = 'openrouter' AND adapter_credentials::jsonb ? 'xTitle';
