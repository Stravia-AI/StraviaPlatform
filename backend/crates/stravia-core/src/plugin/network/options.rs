use stravia_vendor_sdk::net::{self, OptionError};

use super::shape::ShapeRegistry;

/// Apply the SDK grammar and resolve named shapes at the native host boundary.
/// This must be used both for option setters and for request snapshots.
pub(super) fn validate_option(
    registry: &ShapeRegistry,
    key: &str,
    value: &str,
) -> Result<(), OptionError> {
    net::validate_host_option(key, value)?;
    if key == net::keys::SHAPE && registry.get(value).is_none() {
        return Err(OptionError {
            key: key.to_owned(),
            value: value.to_owned(),
        });
    }
    Ok(())
}
