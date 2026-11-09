//! Portable egress shape declarations. The host validates and compiles these settings.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressShape {
    pub name: String,
    pub revision: u64,
    #[serde(default)]
    pub source: Option<ShapeSource>,
    pub tls: TlsShape,
    #[serde(default)]
    pub http1: Http1Shape,
    #[serde(default)]
    pub http2: Http2Shape,
    #[serde(default)]
    pub ws: WsShape,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeSource {
    pub client: String,
    pub runtime: String,
    pub captured: String,
    pub ja4: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsShape {
    pub versions: Vec<String>,
    pub cipher_suites: Vec<String>,
    pub advertise_only_cipher_suites: Vec<String>,
    pub groups: Vec<String>,
    pub key_shares: Vec<String>,
    pub signature_algorithms: Vec<String>,
    #[serde(deserialize_with = "deserialize_extension_order")]
    pub extension_order: Option<Vec<u16>>,
    pub omit_extensions: Vec<u16>,
    pub raw_extensions: Vec<RawExtensionShape>,
    pub padding: Option<String>,
    pub renegotiation: Option<String>,
    pub ec_point_formats: Option<Vec<u8>>,
    pub cert_compression: Vec<String>,
    #[serde(default = "default_true")]
    pub session_tickets: bool,
    pub alpn: AlpnShape,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExtensionShape {
    #[serde(rename = "type")]
    pub typ: u16,
    #[serde(
        deserialize_with = "deserialize_bytes",
        serialize_with = "serialize_bytes"
    )]
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlpnShape {
    pub http: Vec<String>,
    pub ws: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http1Shape {
    pub slot_positions: SlotPositions,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlotPositions {
    pub host: Option<String>,
    pub content_length: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http2Shape {
    pub settings: Vec<(String, u32)>,
    pub early_frames: Vec<EarlyFrameShape>,
    pub pseudo_order: Vec<String>,
    pub content_length: String,
    pub keepalive: KeepaliveShape,
}

impl Default for Http2Shape {
    fn default() -> Self {
        Self {
            settings: Vec::new(),
            early_frames: Vec::new(),
            pseudo_order: Vec::new(),
            content_length: "auto".into(),
            keepalive: KeepaliveShape::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EarlyFrameShape {
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    Priority {
        stream_id: u32,
        dependency_id: u32,
        weight: u8,
        exclusive: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeepaliveShape {
    pub interval_ms: Option<u64>,
    pub timeout_ms: u64,
    pub while_idle: bool,
}

impl Default for KeepaliveShape {
    fn default() -> Self {
        Self {
            interval_ms: Some(15_000),
            timeout_ms: 10_000,
            while_idle: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WsShape {
    pub over_h2: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExtensionOrderInput {
    Random(String),
    List(Vec<WireU16>),
}
#[derive(Deserialize)]
#[serde(untagged)]
enum WireU16 {
    Number(u16),
    Hex(String),
}

fn deserialize_extension_order<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<u16>>, D::Error> {
    use serde::de::Error as _;
    match Option::<ExtensionOrderInput>::deserialize(deserializer)? {
        None => Ok(None),
        Some(ExtensionOrderInput::Random(s)) if s == "random" => Ok(None),
        Some(ExtensionOrderInput::Random(_)) => Err(D::Error::custom(
            "expected random or an ordered extension list",
        )),
        Some(ExtensionOrderInput::List(values)) => values
            .into_iter()
            .map(|v| match v {
                WireU16::Number(n) => Ok(n),
                WireU16::Hex(s) => u16::from_str_radix(
                    s.strip_prefix("0x")
                        .ok_or_else(|| D::Error::custom("extension type must be 0x-prefixed"))?,
                    16,
                )
                .map_err(D::Error::custom),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
    }
}

fn deserialize_bytes<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    use serde::de::Error as _;
    let s = String::deserialize(deserializer)?;
    let hex = s.strip_prefix("0x").unwrap_or(&s);
    if !hex.len().is_multiple_of(2) || !hex.is_ascii() {
        return Err(D::Error::custom("expected an even-length hex string"));
    }
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(D::Error::custom)?;
            u8::from_str_radix(pair, 16).map_err(D::Error::custom)
        })
        .collect()
}

fn serialize_bytes<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    struct Hex<'a>(&'a [u8]);
    impl std::fmt::Display for Hex<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for byte in self.0 {
                write!(f, "{byte:02x}")?;
            }
            Ok(())
        }
    }
    serializer.collect_str(&Hex(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_extension_bytes_round_trip_as_hex() {
        let extension: RawExtensionShape =
            serde_json::from_str(r#"{"type":123,"data":"0x00aBff"}"#).unwrap();
        assert_eq!(extension.data, [0, 171, 255]);
        let json = serde_json::to_string(&extension).unwrap();
        assert_eq!(json, r#"{"type":123,"data":"00abff"}"#);
        assert_eq!(
            serde_json::from_str::<RawExtensionShape>(&json).unwrap(),
            extension
        );
        assert!(serde_json::from_str::<RawExtensionShape>(r#"{"type":123,"data":"abc"}"#).is_err());
        assert!(serde_json::from_str::<RawExtensionShape>(r#"{"type":123,"data":"zz"}"#).is_err());
    }

    #[test]
    fn shape_defaults_and_extension_order_round_trip() {
        let shape: EgressShape = serde_json::from_str(
            r#"{"name":"example","revision":1,"tls":{"extension_order":["0x000a",11]}}"#,
        )
        .unwrap();
        assert_eq!(shape.tls.extension_order, Some(vec![10, 11]));
        assert!(shape.tls.session_tickets);
        assert_eq!(shape.http2.content_length, "auto");
        assert_eq!(
            serde_json::from_str::<EgressShape>(&serde_json::to_string(&shape).unwrap()).unwrap(),
            shape
        );
    }
}
