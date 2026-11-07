use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use rustls::{
    CertificateCompressionAlgorithm, CipherSuite, ClientConfig, DigitallySignedStruct,
    DistinguishedName, Error, SignatureScheme,
    client::{
        ClientHelloShape, PaddingPolicy, RawExtension, Renegotiation, Resumption,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, ServerName, UnixTime},
};

use super::{ShapeError, TlsShape};

#[derive(Debug)]
struct BrotliDecompressor;
#[derive(Debug)]
struct ZlibDecompressor;

impl rustls::compress::CertDecompressor for BrotliDecompressor {
    fn algorithm(&self) -> CertificateCompressionAlgorithm {
        CertificateCompressionAlgorithm::Brotli
    }
    fn decompress(
        &self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), rustls::compress::DecompressionFailed> {
        use brotli_decompressor::{
            BrotliDecompressStream, BrotliResult, BrotliState, StandardAlloc,
        };
        let mut state = BrotliState::new_strict(
            StandardAlloc::default(),
            StandardAlloc::default(),
            StandardAlloc::default(),
        );
        let (mut input_left, mut input_pos) = (input.len(), 0);
        let (mut output_left, mut output_pos, mut total) = (output.len(), 0, 0);
        match BrotliDecompressStream(
            &mut input_left,
            &mut input_pos,
            input,
            &mut output_left,
            &mut output_pos,
            output,
            &mut total,
            &mut state,
        ) {
            BrotliResult::ResultSuccess if input_left == 0 && output_left == 0 => Ok(()),
            _ => Err(rustls::compress::DecompressionFailed),
        }
    }
}

impl rustls::compress::CertDecompressor for ZlibDecompressor {
    fn algorithm(&self) -> CertificateCompressionAlgorithm {
        CertificateCompressionAlgorithm::Zlib
    }
    fn decompress(
        &self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), rustls::compress::DecompressionFailed> {
        let mut decoder = flate2::Decompress::new(true);
        let status = decoder
            .decompress(input, output, flate2::FlushDecompress::Finish)
            .map_err(|_| rustls::compress::DecompressionFailed)?;
        if status == flate2::Status::StreamEnd
            && decoder.total_in() == input.len() as u64
            && decoder.total_out() == output.len() as u64
        {
            Ok(())
        } else {
            Err(rustls::compress::DecompressionFailed)
        }
    }
}

static BROTLI: BrotliDecompressor = BrotliDecompressor;
static ZLIB: ZlibDecompressor = ZlibDecompressor;

// Keep tickets but prevent a cached server preference from changing an explicitly
// configured initial key share on subsequent connections.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SessionPolicy {
    Disabled,
    FixedShare,
    Standard,
}

impl SessionPolicy {
    pub(crate) fn resumption(self) -> Resumption {
        match self {
            Self::Disabled => Resumption::disabled(),
            Self::FixedShare => Resumption::store(Arc::new(FixedShareStore(
                rustls::client::ClientSessionMemoryCache::new(256),
            ))),
            Self::Standard => Resumption::in_memory_sessions(256),
        }
    }
}

#[derive(Debug)]
struct FixedShareStore(rustls::client::ClientSessionMemoryCache);
impl rustls::client::ClientSessionStore for FixedShareStore {
    fn set_kx_hint(&self, _: ServerName<'static>, _: rustls::NamedGroup) {}
    fn kx_hint(&self, _: &ServerName<'_>) -> Option<rustls::NamedGroup> {
        None
    }
    fn set_tls12_session(
        &self,
        name: ServerName<'static>,
        value: rustls::client::Tls12ClientSessionValue,
    ) {
        self.0.set_tls12_session(name, value);
    }
    fn tls12_session(
        &self,
        name: &ServerName<'_>,
    ) -> Option<rustls::client::Tls12ClientSessionValue> {
        self.0.tls12_session(name)
    }
    fn remove_tls12_session(&self, name: &ServerName<'static>) {
        self.0.remove_tls12_session(name);
    }
    fn insert_tls13_ticket(
        &self,
        name: ServerName<'static>,
        value: rustls::client::Tls13ClientSessionValue,
    ) {
        self.0.insert_tls13_ticket(name, value);
    }
    fn take_tls13_ticket(
        &self,
        name: &ServerName<'static>,
    ) -> Option<rustls::client::Tls13ClientSessionValue> {
        self.0.take_tls13_ticket(name)
    }
}

/// Resolved cryptographic settings; each proxy and protocol gets its own session store.
pub(super) struct ResolvedTls {
    base: ClientConfig,
    http_alpn: Vec<Vec<u8>>,
    ws_alpn: Vec<Vec<u8>>,
    tickets: bool,
    fixed_share: bool,
    cache: Mutex<HashMap<String, [Option<Arc<ClientConfig>>; 2]>>,
}

#[derive(Debug)]
struct ShapeVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for ShapeVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.inner
            .verify_server_cert(cert, intermediates, name, ocsp, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }
    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}

fn hex_id(field: &str, value: &str) -> Result<u16, ShapeError> {
    let digits = value
        .strip_prefix("0x")
        .ok_or_else(|| ShapeError::field(field, format!("expected hexadecimal ID: {value}")))?;
    if digits.len() != 4 {
        return Err(ShapeError::field(
            field,
            format!("expected four hexadecimal digits: {value}"),
        ));
    }
    u16::from_str_radix(digits, 16).map_err(|e| ShapeError::field(field, e))
}

// TLS 1.2 suites seen in OpenSSL/Go/Node fingerprints. These are never
// installed in the provider, so selecting one fails the handshake.
fn safe_advertise_only(id: u16) -> bool {
    matches!(
        id,
        0x002f
            | 0x0032
            | 0x0033
            | 0x0035
            | 0x0038
            | 0x0039
            | 0x003c
            | 0x003d
            | 0x0040
            | 0x0067
            | 0x006a
            | 0x006b
            | 0x009c
            | 0x009d
            | 0x009e
            | 0x009f
            | 0x00a2
            | 0x00a3
            | 0xccaa
            | 0xc009
            | 0xc00a
            | 0xc013
            | 0xc014
            | 0xc023
            | 0xc024
            | 0xc027
            | 0xc028
            | 0xc050
            | 0xc051
            | 0xc052
            | 0xc053
            | 0xc056
            | 0xc057
            | 0xc05c
            | 0xc05d
            | 0xc060
            | 0xc061
            | 0xc09c
            | 0xc09d
            | 0xc09e
            | 0xc09f
            | 0xc0ac
            | 0xc0ad
    )
}

// Additional schemes are wire declarations only: the platform verifier still
// performs every certificate and handshake signature verification.
fn captured_signature_scheme(id: u16) -> bool {
    matches!(
        id,
        0x0201
            | 0x0301
            | 0x0302
            | 0x0303
            | 0x0401
            | 0x0402
            | 0x0403
            | 0x0501
            | 0x0502
            | 0x0503
            | 0x0601
            | 0x0602
            | 0x0603
            | 0x0804
            | 0x0805
            | 0x0806
            | 0x0807
            | 0x0808
            | 0x0809
            | 0x080a
            | 0x080b
            | 0x081a
            | 0x081b
            | 0x081c
            | 0x0904
            | 0x0905
            | 0x0906
    )
}

fn alpn(field: &str, protocols: &[String]) -> Result<Vec<Vec<u8>>, ShapeError> {
    let mut seen = HashSet::new();
    let mut total = 0usize;
    for p in protocols {
        if p.is_empty() || p.len() > 255 || !seen.insert(p) {
            return Err(ShapeError::field(
                field,
                "ALPN protocols must be unique and 1–255 bytes",
            ));
        }
        total += 1 + p.len();
    }
    if total > u16::MAX as usize - 2 {
        return Err(ShapeError::field(field, "ALPN list is too long"));
    }
    Ok(protocols.iter().map(|p| p.as_bytes().to_vec()).collect())
}

impl ResolvedTls {
    pub(super) fn build(
        shape: &TlsShape,
        inner: Arc<dyn ServerCertVerifier>,
    ) -> Result<Self, ShapeError> {
        let mut provider = rustls::crypto::aws_lc_rs::default_provider();
        let available = rustls::crypto::aws_lc_rs::ALL_CIPHER_SUITES;
        let mut advertised_only = HashSet::new();
        for value in &shape.advertise_only_cipher_suites {
            let id = hex_id("advertise_only_cipher_suites", value)?;
            if !safe_advertise_only(id)
                || available.iter().any(|s| u16::from(s.suite()) == id)
                || !advertised_only.insert(id)
            {
                return Err(ShapeError::field(
                    "advertise_only_cipher_suites",
                    format!("invalid or duplicate advertise-only suite {value}"),
                ));
            }
        }
        let suites = if shape.cipher_suites.is_empty() {
            if !advertised_only.is_empty() {
                return Err(ShapeError::field(
                    "advertise_only_cipher_suites",
                    "requires an explicit cipher_suites list",
                ));
            }
            None
        } else {
            let mut seen = HashSet::new();
            let mut implemented = Vec::new();
            let mut wire = Vec::new();
            for value in &shape.cipher_suites {
                let id = hex_id("cipher_suites", value)?;
                if !seen.insert(id) {
                    return Err(ShapeError::field(
                        "cipher_suites",
                        format!("duplicate suite {value}"),
                    ));
                }
                if let Some(suite) = available.iter().find(|s| u16::from(s.suite()) == id) {
                    if advertised_only.contains(&id) {
                        return Err(ShapeError::field(
                            "advertise_only_cipher_suites",
                            format!("implemented suite {value}"),
                        ));
                    }
                    implemented.push(*suite);
                } else if id != 0x00ff && !advertised_only.contains(&id) {
                    return Err(ShapeError::field(
                        "cipher_suites",
                        format!("unsupported suite {value} is not explicitly allowed"),
                    ));
                }
                wire.push(CipherSuite::from(id));
            }
            if !advertised_only.is_subset(&seen) {
                return Err(ShapeError::field(
                    "advertise_only_cipher_suites",
                    "every entry must appear in cipher_suites",
                ));
            }
            if implemented.is_empty() {
                return Err(ShapeError::field(
                    "cipher_suites",
                    "at least one implemented suite is required",
                ));
            }
            provider.cipher_suites = implemented;
            Some(wire)
        };
        let supported_groups = if shape.groups.is_empty() {
            None
        } else {
            use rustls::crypto::aws_lc_rs::kx_group;
            let mut seen = HashSet::new();
            let mut implemented = Vec::new();
            let mut wire = Vec::new();
            for name in &shape.groups {
                if !seen.insert(name) {
                    return Err(ShapeError::field(
                        "groups",
                        format!("duplicate group {name}"),
                    ));
                }
                let (id, group) = match name.as_str() {
                    "X25519" => (29, Some(kx_group::X25519)),
                    "X25519MLKEM768" => (0x11ec, Some(kx_group::X25519MLKEM768)),
                    "SECP256R1MLKEM768" => (0x11eb, Some(kx_group::SECP256R1MLKEM768)),
                    "secp256r1" => (23, Some(kx_group::SECP256R1)),
                    "secp384r1" => (24, Some(kx_group::SECP384R1)),
                    "SECP384R1MLKEM1024" => (0x11ed, Some(super::kx::SECP384R1MLKEM1024)),
                    // These names are only offered, not installed as key exchanges.
                    "X448" => (30, None),
                    "secp521r1" => (25, None),
                    "ffdhe2048" => (256, None),
                    "ffdhe3072" => (257, None),
                    _ => {
                        return Err(ShapeError::field(
                            "groups",
                            format!("unsupported group {name}"),
                        ));
                    }
                };
                wire.push(rustls::NamedGroup::from(id));
                if let Some(group) = group {
                    implemented.push(group);
                }
            }
            if implemented.is_empty() {
                return Err(ShapeError::field(
                    "groups",
                    "at least one implemented group is required",
                ));
            }
            provider.kx_groups = implemented;
            Some(wire)
        };
        if !shape.key_shares.is_empty() {
            if !shape.versions.is_empty() && !shape.versions.iter().any(|v| v == "1.3") {
                return Err(ShapeError::field(
                    "key_shares",
                    "key shares require TLS 1.3",
                ));
            }
            let requested = shape
                .key_shares
                .iter()
                .map(|name| match name.as_str() {
                    "X25519" => Ok(rustls::NamedGroup::X25519),
                    "X25519MLKEM768" => Ok(rustls::NamedGroup::X25519MLKEM768),
                    "SECP256R1MLKEM768" => Ok(rustls::NamedGroup::from(0x11eb)),
                    "secp256r1" => Ok(rustls::NamedGroup::secp256r1),
                    "secp384r1" => Ok(rustls::NamedGroup::secp384r1),
                    "SECP384R1MLKEM1024" => Ok(rustls::NamedGroup::from(0x11ed)),
                    _ => Err(ShapeError::field(
                        "key_shares",
                        format!("unsupported group {name}"),
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let selected = provider
                .kx_groups
                .iter()
                .position(|group| group.name() == requested[0])
                .ok_or_else(|| {
                    ShapeError::field(
                        "key_shares",
                        "initial share must be an implemented configured group",
                    )
                })?;
            // rustls uses its first group for the initial share; the hook keeps
            // the independently captured supported_groups wire order intact.
            provider.kx_groups[..=selected].rotate_right(1);
            let active = provider.kx_groups[0]
                .start()
                .map_err(|e| ShapeError::field("key_shares", e))?;
            let mut actual = vec![active.group()];
            if let Some((group, _)) = active.hybrid_component()
                && provider.kx_groups.iter().any(|g| {
                    g.name() == group && g.usable_for_version(rustls::ProtocolVersion::TLSv1_3)
                })
            {
                actual.push(group);
            }
            if requested != actual {
                return Err(ShapeError::field(
                    "key_shares",
                    "requested shares do not match the configured key exchange groups",
                ));
            }
        }
        let versions = if shape.versions.is_empty() {
            rustls::DEFAULT_VERSIONS.to_vec()
        } else {
            let mut seen = HashSet::new();
            shape
                .versions
                .iter()
                .map(|v| {
                    if !seen.insert(v) {
                        return Err(ShapeError::field(
                            "versions",
                            format!("duplicate version {v}"),
                        ));
                    }
                    match v.as_str() {
                        "1.2" => Ok(&rustls::version::TLS12),
                        "1.3" => Ok(&rustls::version::TLS13),
                        _ => Err(ShapeError::field(
                            "versions",
                            format!("unsupported version {v}"),
                        )),
                    }
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let provider = Arc::new(provider);
        let supported = inner.supported_verify_schemes();
        let schemes = if shape.signature_algorithms.is_empty() {
            supported
        } else {
            let mut seen = HashSet::new();
            shape
                .signature_algorithms
                .iter()
                .map(|v| {
                    let id = hex_id("signature_algorithms", v)?;
                    let scheme = SignatureScheme::from(id);
                    if !seen.insert(id)
                        || (!supported.contains(&scheme) && !captured_signature_scheme(id))
                    {
                        return Err(ShapeError::field(
                            "signature_algorithms",
                            format!("unsupported or duplicate signature algorithm {v}"),
                        ));
                    }
                    Ok(scheme)
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let verifier = Arc::new(ShapeVerifier { inner, schemes });
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&versions)
            .map_err(|e| ShapeError::field("versions", e))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        let mut compression_seen = HashSet::new();
        config.cert_decompressors = shape
            .cert_compression
            .iter()
            .map(|name| {
                if !compression_seen.insert(name) {
                    return Err(ShapeError::field(
                        "cert_compression",
                        format!("duplicate algorithm {name}"),
                    ));
                }
                let decompressor: &'static dyn rustls::compress::CertDecompressor =
                    match name.as_str() {
                        "brotli" => &BROTLI,
                        "zlib" => &ZLIB,
                        _ => {
                            return Err(ShapeError::field(
                                "cert_compression",
                                format!("unsupported algorithm {name}"),
                            ));
                        }
                    };
                Ok(decompressor)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let padding = match shape.padding.as_deref() {
            None => None,
            Some("boringssl") => Some(PaddingPolicy::BoringSsl),
            Some("none") => Some(PaddingPolicy::None),
            Some(v) => return Err(ShapeError::field("padding", format!("unknown policy {v}"))),
        };
        let renegotiation = match shape.renegotiation.as_deref() {
            None | Some("default") => Renegotiation::Default,
            Some("scsv") => Renegotiation::Scsv,
            Some("extension") => Renegotiation::Extension,
            Some("omit") => Renegotiation::Omit,
            Some(v) => {
                return Err(ShapeError::field(
                    "renegotiation",
                    format!("unknown policy {v}"),
                ));
            }
        };
        let mut raw_seen = HashSet::new();
        let mut raw_extensions = Vec::with_capacity(shape.raw_extensions.len());
        for ext in &shape.raw_extensions {
            let valid = match ext.typ {
                18 | 22 => ext.data.is_empty(),
                50 => {
                    ext.data.len() >= 4
                        && ext.data.len() <= u16::MAX as usize
                        && u16::from_be_bytes([ext.data[0], ext.data[1]]) as usize
                            == ext.data.len() - 2
                        && (ext.data.len() - 2) % 2 == 0
                }
                _ => false,
            };
            if !valid || !raw_seen.insert(ext.typ) {
                return Err(ShapeError::field(
                    "raw_extensions",
                    format!("invalid or duplicate extension {}", ext.typ),
                ));
            }
            raw_extensions.push(RawExtension {
                typ: ext.typ,
                data: ext.data.clone(),
            });
        }
        if let Some(order) = &shape.extension_order {
            let mut seen = HashSet::new();
            if order.iter().any(|id| !seen.insert(*id))
                || order
                    .iter()
                    .position(|id| *id == 41)
                    .is_some_and(|i| i + 1 != order.len())
            {
                return Err(ShapeError::field(
                    "extension_order",
                    "duplicate extension or pre_shared_key is not last",
                ));
            }
        }
        if let Some(formats) = &shape.ec_point_formats
            && (formats.is_empty() || formats.len() > 255 || !formats.contains(&0))
        {
            return Err(ShapeError::field(
                "ec_point_formats",
                "must contain uncompressed format and fit in 255 bytes",
            ));
        }
        let mut omitted = HashSet::new();
        for typ in &shape.omit_extensions {
            if !matches!(*typ, 5 | 45) || !omitted.insert(*typ) {
                return Err(ShapeError::field(
                    "omit_extensions",
                    format!("unsupported or duplicate extension {typ}"),
                ));
            }
            if *typ == 45 && shape.session_tickets {
                return Err(ShapeError::field(
                    "omit_extensions",
                    "omitting PSK modes requires session tickets to be disabled",
                ));
            }
        }
        let hook = ClientHelloShape {
            cipher_suites: suites,
            supported_groups,
            extension_order: shape.extension_order.clone(),
            raw_extensions,
            omit_extensions: shape.omit_extensions.clone(),
            padding,
            renegotiation,
            ec_point_formats: shape.ec_point_formats.clone(),
        };
        hook.validate(&config, false)
            .map_err(|e| ShapeError::field("tls", e))?;
        config.client_hello_shape = Some(Arc::new(hook));
        Ok(Self {
            base: config,
            http_alpn: alpn("alpn.http", &shape.alpn.http)?,
            ws_alpn: alpn("alpn.ws", &shape.alpn.ws)?,
            tickets: shape.session_tickets,
            fixed_share: !shape.key_shares.is_empty(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn session_policy(&self) -> SessionPolicy {
        if !self.tickets {
            SessionPolicy::Disabled
        } else if self.fixed_share {
            SessionPolicy::FixedShare
        } else {
            SessionPolicy::Standard
        }
    }

    pub(super) fn tls_for(&self, proxy_identity: &str, websocket: bool) -> Arc<ClientConfig> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = usize::from(websocket);
        if let Some(config) = cache
            .get(proxy_identity)
            .and_then(|configs| configs[index].as_ref())
        {
            return Arc::clone(config);
        }
        let mut config = self.base.clone();
        config.alpn_protocols = if websocket {
            self.ws_alpn.clone()
        } else {
            self.http_alpn.clone()
        };
        config.resumption = self.session_policy().resumption();
        let config = Arc::new(config);
        if let Some(configs) = cache.get_mut(proxy_identity) {
            configs[index] = Some(Arc::clone(&config));
        } else {
            let mut configs = [None, None];
            configs[index] = Some(Arc::clone(&config));
            cache.insert(proxy_identity.to_owned(), configs);
        }
        config
    }
}
