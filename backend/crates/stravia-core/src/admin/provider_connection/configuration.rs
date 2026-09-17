pub(super) fn validate_adapter_credentials(
    vendor_id: &str,
    values: std::collections::BTreeMap<String, String>,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let vendor = crate::provider::VendorRegistry::global()
        .get_vendor(vendor_id)
        .ok_or_else(|| anyhow::anyhow!("Vendor `{vendor_id}` is not installed"))?;
    vendor.validate_credentials(values)
}

/// Validate `vendor_options` against the vendor's declared `option_fields`:
/// unknown keys and values that do not match the declared input kind are
/// rejected so typos fail loudly instead of silently no-op'ing at runtime.
pub(super) fn validate_vendor_options(
    vendor_id: &str,
    values: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let vendor = crate::provider::VendorRegistry::global()
        .get_vendor(vendor_id)
        .ok_or_else(|| anyhow::anyhow!("Vendor `{vendor_id}` is not installed"))?;
    let declared = vendor
        .metadata()
        .map(|metadata| metadata.option_fields)
        .unwrap_or(&[]);
    for field in declared {
        if let Some(value) = values.get(field.key) {
            let type_ok = match field.input {
                crate::provider::metadata::OptionInputKind::Toggle => value.is_boolean(),
            };
            if !type_ok {
                anyhow::bail!(
                    "Vendor option `{}` must be a boolean for vendor `{vendor_id}`",
                    field.key
                );
            }
        }
    }
    for key in values.keys() {
        if !declared.iter().any(|field| field.key == key) {
            anyhow::bail!("Vendor `{vendor_id}` does not declare option `{key}`");
        }
    }
    Ok(values)
}

pub(super) fn assemble_vendor_base_url(
    vendor_id: &str,
    credentials: &std::collections::BTreeMap<String, String>,
    configured_base_url: Option<&str>,
) -> anyhow::Result<String> {
    crate::provider::VendorRegistry::global()
        .get_vendor(vendor_id)
        .ok_or_else(|| anyhow::anyhow!("Vendor `{vendor_id}` is not installed"))?
        .assemble_base_url(credentials, configured_base_url)
}

pub(super) fn validate_provider_base_url(value: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| anyhow::anyhow!("Provider Base URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!(
            "Provider Base URL must contain only an HTTP(S) origin and optional base path"
        );
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

#[cfg(test)]
mod image_base_url_tests {
    use super::validate_provider_base_url;

    #[test]
    fn image_provider_base_url_rejects_credential_query_and_fragment_smuggling() {
        for value in [
            "https://user:secret@example.com/v1",
            "https://example.com/v1?api_key=secret",
            "https://example.com/v1#images",
        ] {
            assert!(validate_provider_base_url(value).is_err(), "{value}");
        }
        assert_eq!(
            validate_provider_base_url("https://example.com/base/v1/").unwrap(),
            "https://example.com/base/v1"
        );
    }
}
