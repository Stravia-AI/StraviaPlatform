use url::Url;

use super::FetchError;
use stravia_web_access_contract::address_policy::allows_url;

pub(crate) fn validate_url(value: &str) -> Result<Url, FetchError> {
    let url = Url::parse(value).map_err(|_| FetchError::invalid_url(value))?;
    if !allows_url(&url) {
        return Err(FetchError::invalid_url(value));
    }
    Ok(url)
}

pub(super) fn validate_parsed_url(url: &Url) -> Result<(), FetchError> {
    if !allows_url(url) {
        return Err(FetchError::invalid_url(url.as_str()));
    }
    Ok(())
}
