//! Signed plugin catalog and bounded component distribution format.
use std::collections::HashSet;
use std::io::{Read, Write};

use anyhow::{Context, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(test)]
#[path = "distribution_tests.rs"]
mod tests;

pub const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 256 * 1024;
const MAX_KEY_BYTES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema_version: u32,
    pub repository_id: u64,
    pub release_tag: String,
    pub generated_at: String,
    pub releases: Vec<CatalogRelease>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRelease {
    pub vendor_id: String,
    pub version: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_svg: Option<String>,
    pub published_at: String,
    pub compatibility: Compatibility,
    pub artifact: Artifact,
    pub component: ComponentDigest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    pub host_version_req: String,
    pub wit_version: String,
    pub canonical_format_version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub filename: String,
    pub compression: String,
    pub media_type: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentDigest {
    pub size: u64,
    pub sha256: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut result, "{byte:02x}").expect("writing to a String cannot fail");
    }
    result
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse a bounded v2 catalog. Serde rejects duplicate fields and unknown keys at every level.
pub fn parse_catalog(bytes: &[u8]) -> anyhow::Result<Catalog> {
    ensure!(bytes.len() <= MAX_CATALOG_BYTES, "catalog exceeds 256 KiB");
    let catalog: Catalog = serde_json::from_slice(bytes).context("invalid catalog JSON")?;
    ensure!(catalog.schema_version == 2, "unsupported catalog schema");
    ensure!(catalog.repository_id != 0, "invalid repository ID");
    ensure!(
        !catalog.release_tag.trim().is_empty()
            && !catalog.release_tag.chars().any(char::is_control),
        "invalid release tag"
    );
    chrono::DateTime::parse_from_rfc3339(&catalog.generated_at).context("invalid generated_at")?;
    ensure!(catalog.releases.len() <= 100, "too many releases");
    let mut identities = HashSet::new();
    let mut filenames = HashSet::new();
    for release in &catalog.releases {
        ensure!(
            !release.vendor_id.is_empty()
                && release.vendor_id != "base"
                && release.vendor_id.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'.' | b'-' | b'_')),
            "invalid vendor ID"
        );
        semver::Version::parse(&release.version).context("invalid plugin version")?;
        ensure!(
            identities.insert((&release.vendor_id, &release.version)),
            "duplicate plugin version"
        );
        ensure!(
            !release.display_name.trim().is_empty(),
            "empty display name"
        );
        ensure!(
            release
                .icon_svg
                .as_ref()
                .is_none_or(|svg| svg.len() <= 32 * 1024),
            "icon exceeds 32 KiB"
        );
        chrono::DateTime::parse_from_rfc3339(&release.published_at)
            .context("invalid published_at")?;
        semver::VersionReq::parse(&release.compatibility.host_version_req)
            .context("invalid host version requirement")?;
        semver::Version::parse(&release.compatibility.wit_version)
            .context("invalid WIT version")?;
        ensure!(
            release.compatibility.canonical_format_version > 0,
            "invalid canonical format version"
        );
        let artifact = &release.artifact;
        ensure!(
            !artifact.filename.is_empty()
                && artifact.filename != "."
                && artifact.filename != ".."
                && !artifact
                    .filename
                    .chars()
                    .any(|c| c.is_control() || c == '/' || c == '\\'),
            "invalid artifact filename"
        );
        ensure!(
            filenames.insert(&artifact.filename),
            "duplicate artifact filename"
        );
        ensure!(
            artifact.compression == "zstd" && artifact.media_type == "application/zstd",
            "unsupported artifact format"
        );
        ensure!(
            artifact.size > 0
                && artifact.size <= MAX_COMPONENT_BYTES as u64
                && valid_digest(&artifact.sha256),
            "invalid artifact digest or size"
        );
        ensure!(
            release.component.size > 0
                && release.component.size <= MAX_COMPONENT_BYTES as u64
                && valid_digest(&release.component.sha256),
            "invalid component digest or size"
        );
    }
    Ok(catalog)
}

fn key_parts(public_key: &str) -> anyhow::Result<(minisign_verify::PublicKey, Vec<u8>)> {
    ensure!(
        public_key.len() <= MAX_KEY_BYTES,
        "public key exceeds 4 KiB"
    );
    let mut lines = public_key.lines();
    let first = lines.next().context("missing public key")?;
    let encoded = if first.starts_with("untrusted comment:") {
        lines.next().context("missing public key data")?
    } else {
        first
    };
    ensure!(lines.next().is_none(), "unexpected public key data");
    let key = minisign_verify::PublicKey::from_base64(encoded).context("invalid public key")?;
    let decoded = STANDARD
        .decode(encoded)
        .context("invalid public key encoding")?;
    Ok((key, decoded))
}

pub fn public_key_fingerprint(public_key: &str) -> anyhow::Result<String> {
    let (_, decoded) = key_parts(public_key)?;
    Ok(sha256_hex(&decoded))
}

/// Authenticate exact catalog bytes before interpreting any JSON.
pub fn verify_catalog(bytes: &[u8], signature: &str, public_key: &str) -> anyhow::Result<Catalog> {
    ensure!(bytes.len() <= MAX_CATALOG_BYTES, "catalog exceeds 256 KiB");
    ensure!(signature.len() <= MAX_KEY_BYTES, "signature exceeds 4 KiB");
    let (key, _) = key_parts(public_key)?;
    let signature = minisign_verify::Signature::decode(signature).context("invalid signature")?;
    key.verify(bytes, &signature, false)
        .context("catalog signature verification failed")?;
    parse_catalog(bytes)
}

/// Decode exactly one standard, dictionary-free zstd frame with bounded output.
pub fn decode_component(
    bytes: &[u8],
    expected: Option<&ComponentDigest>,
) -> anyhow::Result<Vec<u8>> {
    ensure!(
        bytes.len() <= MAX_COMPONENT_BYTES,
        "compressed component exceeds 64 MiB"
    );
    ensure!(
        bytes.len() >= 5 && bytes[..4] == [0x28, 0xb5, 0x2f, 0xfd],
        "not a standard zstd frame"
    );
    ensure!(bytes[4] & 0x03 == 0, "zstd dictionaries are not supported");
    let frame_size = zstd::zstd_safe::find_frame_compressed_size(bytes)
        .map_err(|code| anyhow::anyhow!("invalid zstd frame: {code:?}"))?;
    ensure!(
        frame_size == bytes.len(),
        "trailing data or multiple zstd frames"
    );
    let limit = match expected {
        Some(expected) => {
            ensure!(
                expected.size <= MAX_COMPONENT_BYTES as u64 && valid_digest(&expected.sha256),
                "invalid expected component digest"
            );
            expected.size as usize
        }
        None => MAX_COMPONENT_BYTES,
    };
    let mut decoder = zstd::stream::read::Decoder::new(bytes)
        .context("invalid zstd frame")?
        .single_frame();
    decoder
        .window_log_max(23)
        .context("setting zstd window limit")?;
    let mut output = Vec::new();
    decoder
        .take(limit as u64 + 1)
        .read_to_end(&mut output)
        .context("decompressing component")?;
    ensure!(
        output.len() <= limit,
        "decompressed component exceeds size limit"
    );
    if let Some(expected) = expected {
        ensure!(
            output.len() as u64 == expected.size && sha256_hex(&output) == expected.sha256,
            "component digest or size mismatch"
        );
    }
    Ok(output)
}

pub fn encode_component(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_COMPONENT_BYTES,
        "invalid component size"
    );
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 19)?;
    encoder.window_log(23)?;
    encoder.write_all(bytes)?;
    let compressed = encoder.finish()?;
    ensure!(
        compressed.len() <= MAX_COMPONENT_BYTES,
        "compressed component exceeds 64 MiB"
    );
    let digest = ComponentDigest {
        size: bytes.len() as u64,
        sha256: sha256_hex(bytes),
    };
    ensure!(
        decode_component(&compressed, Some(&digest))? == bytes,
        "component compression round-trip failed"
    );
    Ok(compressed)
}
