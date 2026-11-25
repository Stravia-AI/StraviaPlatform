//! 重试身份读取整个 canonical 请求，仅排除本次请求的关联 ID。
//! 每个叶值直接写入摘要；无序对象只保留固定大小的键/值摘要，不复制历史正文。

use super::{AiRequest, sha256_slices, with_serialized_json};
use serde::Serialize;
use serde::ser::{
    SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant,
};

type Hash = [u8; 32];
type Result<T, E = serde_json::Error> = std::result::Result<T, E>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    Request,
    Parameters,
    Session,
    Other,
}

struct Keys {
    parameters: Hash,
    session: Hash,
    request_id: Hash,
}

fn scalar(value: &(impl Serialize + ?Sized)) -> Result<Hash> {
    with_serialized_json(value, |bytes| sha256_slices(&[b"scalar:", bytes]))
}

pub(super) fn request_hash(request: &AiRequest) -> Result<Hash> {
    let keys = Keys {
        parameters: scalar("parameters")?,
        session: scalar("session")?,
        request_id: scalar("request_id")?,
    };
    request.serialize(Identity {
        path: Path::Request,
        keys: &keys,
    })
}

struct Identity<'a> {
    path: Path,
    keys: &'a Keys,
}

impl<'a> Identity<'a> {
    fn other(keys: &'a Keys) -> Self {
        Self {
            path: Path::Other,
            keys,
        }
    }
}

macro_rules! scalar_methods {
    ($($name:ident($ty:ty)),* $(,)?) => {
        $(fn $name(self, value: $ty) -> Result<Hash> { scalar(&value) })*
    };
}

impl<'a> serde::Serializer for Identity<'a> {
    type Ok = Hash;
    type Error = serde_json::Error;
    type SerializeSeq = Sequence<'a>;
    type SerializeTuple = Sequence<'a>;
    type SerializeTupleStruct = Sequence<'a>;
    type SerializeTupleVariant = Sequence<'a>;
    type SerializeMap = Object<'a>;
    type SerializeStruct = Object<'a>;
    type SerializeStructVariant = Object<'a>;

    scalar_methods! {
        serialize_bool(bool), serialize_i8(i8), serialize_i16(i16), serialize_i32(i32),
        serialize_i64(i64), serialize_i128(i128), serialize_u8(u8), serialize_u16(u16),
        serialize_u32(u32), serialize_u64(u64), serialize_u128(u128), serialize_f32(f32),
        serialize_f64(f64), serialize_char(char), serialize_str(&str),
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<Hash> {
        let mut sequence = Sequence::new(self.keys, None);
        for value in value {
            sequence.element(value)?;
        }
        sequence.finish()
    }

    fn serialize_none(self) -> Result<Hash> {
        scalar(&())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Hash> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Hash> {
        scalar(&())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Hash> {
        scalar(&())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Hash> {
        scalar(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Hash> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Hash> {
        let value = value.serialize(Self::other(self.keys))?;
        variant_hash(variant, value)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Sequence<'a>> {
        Ok(Sequence::new(self.keys, None))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Sequence<'a>> {
        Ok(Sequence::new(self.keys, None))
    }

    fn serialize_tuple_struct(self, _name: &'static str, _len: usize) -> Result<Sequence<'a>> {
        Ok(Sequence::new(self.keys, None))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Sequence<'a>> {
        Ok(Sequence::new(self.keys, Some(variant)))
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Object<'a>> {
        Ok(Object::new(self, len.unwrap_or(0), None, None))
    }

    fn serialize_struct(self, name: &'static str, len: usize) -> Result<Object<'a>> {
        // 类型域也区分 serde 的私有 Number 表示与同形业务 JSON，避免身份碰撞。
        Ok(Object::new(self, len, Some(name), None))
    }

    fn serialize_struct_variant(
        self,
        name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Object<'a>> {
        Ok(Object::new(self, len, Some(name), Some(variant)))
    }
}

struct Sequence<'a> {
    buf: Vec<u8>,
    count: u64,
    keys: &'a Keys,
    variant: Option<&'static str>,
}

impl<'a> Sequence<'a> {
    fn new(keys: &'a Keys, variant: Option<&'static str>) -> Self {
        let mut buf = Vec::with_capacity(9 + 64);
        buf.extend_from_slice(b"sequence:");
        Self {
            buf,
            count: 0,
            keys,
            variant,
        }
    }

    fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        let digest = value.serialize(Identity::other(self.keys))?;
        self.buf.extend_from_slice(&digest);
        self.count += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<Hash> {
        self.buf.extend_from_slice(&self.count.to_le_bytes());
        let hash = sha256_slices(&[&self.buf]);
        match self.variant {
            Some(variant) => variant_hash(variant, hash),
            None => Ok(hash),
        }
    }
}

macro_rules! sequence_impl {
    ($trait:ident, $method:ident) => {
        impl $trait for Sequence<'_> {
            type Ok = Hash;
            type Error = serde_json::Error;
            fn $method<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
                self.element(value)
            }
            fn end(self) -> Result<Hash> {
                self.finish()
            }
        }
    };
}

sequence_impl!(SerializeSeq, serialize_element);
sequence_impl!(SerializeTuple, serialize_element);
sequence_impl!(SerializeTupleStruct, serialize_field);
sequence_impl!(SerializeTupleVariant, serialize_field);

struct Object<'a> {
    identity: Identity<'a>,
    entries: Vec<(Hash, Hash)>,
    pending_key: Option<Hash>,
    name: Option<&'static str>,
    variant: Option<&'static str>,
}

impl<'a> Object<'a> {
    fn new(
        identity: Identity<'a>,
        len: usize,
        name: Option<&'static str>,
        variant: Option<&'static str>,
    ) -> Self {
        Self {
            identity,
            entries: Vec::with_capacity(len),
            pending_key: None,
            name,
            variant,
        }
    }

    fn field<T: Serialize + ?Sized>(&mut self, key: Hash, value: &T) -> Result<()> {
        let keys = self.identity.keys;
        if self.identity.path == Path::Session && key == keys.request_id {
            return Ok(());
        }
        let path = match self.identity.path {
            Path::Request if key == keys.parameters => Path::Parameters,
            Path::Parameters if key == keys.session => Path::Session,
            _ => Path::Other,
        };
        self.entries
            .push((key, value.serialize(Identity { path, keys })?));
        Ok(())
    }

    fn finish(mut self) -> Result<Hash> {
        if self.pending_key.is_some() {
            return Err(serde::ser::Error::custom(
                "retry identity map value is missing",
            ));
        }
        // 只排序摘要，HashMap 顺序及 serde_json preserve_order 特性不影响重试身份。
        self.entries.sort_unstable();
        let name = self.name.unwrap_or_default();
        let mut buf = Vec::with_capacity(7 + 8 + name.len() + 8 + self.entries.len() * 64);
        buf.extend_from_slice(b"object:");
        buf.extend_from_slice(&(name.len() as u64).to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (key, value) in self.entries {
            buf.extend_from_slice(&key);
            buf.extend_from_slice(&value);
        }
        let hash = sha256_slices(&[&buf]);
        match self.variant {
            Some(variant) => variant_hash(variant, hash),
            None => Ok(hash),
        }
    }
}

fn variant_hash(variant: &str, value: Hash) -> Result<Hash> {
    let variant_digest = scalar(variant)?;
    Ok(sha256_slices(&[b"variant:", &variant_digest, &value]))
}

impl SerializeMap for Object<'_> {
    type Ok = Hash;
    type Error = serde_json::Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<()> {
        if self.pending_key.is_some() {
            return Err(serde::ser::Error::custom(
                "retry identity map key is repeated",
            ));
        }
        self.pending_key = Some(scalar(key)?);
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        let key = self.pending_key.take().ok_or_else(|| {
            <serde_json::Error as serde::ser::Error>::custom("retry identity map key is missing")
        })?;
        self.field(key, value)
    }

    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(
        &mut self,
        key: &K,
        value: &V,
    ) -> Result<()> {
        self.field(scalar(key)?, value)
    }

    fn end(self) -> Result<Hash> {
        self.finish()
    }
}

impl SerializeStruct for Object<'_> {
    type Ok = Hash;
    type Error = serde_json::Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.field(scalar(key)?, value)
    }

    fn end(self) -> Result<Hash> {
        self.finish()
    }
}

impl SerializeStructVariant for Object<'_> {
    type Ok = Hash;
    type Error = serde_json::Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.field(scalar(key)?, value)
    }

    fn end(self) -> Result<Hash> {
        self.finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    #[test]
    fn unordered_business_schema_and_parameter_maps_have_one_identity() {
        let mut first = AiRequest::new("model", Vec::new());
        first.tools = Some(vec![crate::protocol::ir::ToolSpec {
            name: "lookup".into(),
            description: None,
            parameters: serde_json::from_str(r#"{"outer":{"a":1,"b":[true,null]},"tail":"text"}"#)
                .unwrap(),
            strict: None,
            defer_loading: None,
        }]);
        first.parameters.logit_bias = Some(HashMap::from([("a".into(), 1.0), ("b".into(), 2.0)]));
        first.parameters.metadata = Some(
            [("a".into(), "first".into()), ("b".into(), "second".into())]
                .into_iter()
                .collect(),
        );
        let mut reordered = first.clone();
        reordered.tools.as_mut().unwrap()[0].parameters =
            serde_json::from_str(r#"{"tail":"text","outer":{"b":[true,null],"a":1}}"#).unwrap();
        reordered.parameters.logit_bias =
            Some(HashMap::from([("b".into(), 2.0), ("a".into(), 1.0)]));
        reordered.parameters.metadata = Some(
            [("b".into(), "second".into()), ("a".into(), "first".into())]
                .into_iter()
                .collect(),
        );
        assert_eq!(
            request_hash(&first).unwrap(),
            request_hash(&reordered).unwrap()
        );

        reordered
            .parameters
            .logit_bias
            .as_mut()
            .unwrap()
            .insert("a".into(), 3.0);
        assert_ne!(
            request_hash(&first).unwrap(),
            request_hash(&reordered).unwrap()
        );
    }

    #[test]
    fn only_session_request_id_is_excluded() {
        use crate::protocol::ir::SessionParameters;
        let mut first = AiRequest::new("model", Vec::new());
        first.parameters.session.request_id = Some("attempt-1".into());
        let mut retried = first.clone();
        retried.parameters.session.request_id = Some("attempt-2".into());
        assert_eq!(
            request_hash(&first).unwrap(),
            request_hash(&retried).unwrap()
        );
        retried.parameters.session.request_id = None;
        assert_eq!(
            request_hash(&first).unwrap(),
            request_hash(&retried).unwrap()
        );

        let mutations: [fn(&mut SessionParameters); 7] = [
            |s| s.id = Some("session".into()),
            |s| s.parent_id = Some("parent".into()),
            |s| s.forked_from_id = Some("fork".into()),
            |s| s.context_window_id = Some("window".into()),
            |s| s.turn_id = Some("turn".into()),
            |s| s.parent_turn_id = Some("parent-turn".into()),
            |s| s.root_turn_id = Some("root-turn".into()),
        ];
        for mutate in mutations {
            let mut changed = first.clone();
            mutate(&mut changed.parameters.session);
            assert_ne!(
                request_hash(&first).unwrap(),
                request_hash(&changed).unwrap()
            );
        }

        // 同名业务键不属于会话关联 ID，不能跟随路径过滤一起丢失。
        let mut changed = first.clone();
        changed.parameters.metadata = Some(
            [("request_id".into(), "business-value".into())]
                .into_iter()
                .collect(),
        );
        assert_ne!(
            request_hash(&first).unwrap(),
            request_hash(&changed).unwrap()
        );
    }

    #[test]
    fn inheritance_resets_participate_in_retry_identity() {
        use crate::protocol::ir::ParameterReset;
        let first = AiRequest::new("model", Vec::new());
        let mut changed = first.clone();
        changed.parameters.resets = vec![ParameterReset::NativeTools];
        assert_ne!(
            request_hash(&first).unwrap(),
            request_hash(&changed).unwrap()
        );
        changed.parameters.resets = vec![ParameterReset::ReasoningSummary];
        assert_ne!(
            request_hash(&first).unwrap(),
            request_hash(&changed).unwrap()
        );
    }

    #[test]
    fn business_json_types_order_and_private_number_lookalikes_stay_distinct() {
        let number: serde_json::Value = serde_json::from_str("18446744073709551617").unwrap();
        let payloads = [
            number,
            json!({"$serde_json::private::Number": "18446744073709551617"}),
            json!("18446744073709551617"),
            json!([1, 2]),
            json!([2, 1]),
            json!({"0": 1, "1": 2}),
            json!(null),
            json!("null"),
        ];
        let hashes: Vec<_> = payloads
            .into_iter()
            .map(|payload| {
                let mut request = AiRequest::new("model", Vec::new());
                request.tools = Some(vec![crate::protocol::ir::ToolSpec {
                    name: "lookup".into(),
                    description: None,
                    parameters: payload,
                    strict: None,
                    defer_loading: None,
                }]);
                request_hash(&request).unwrap()
            })
            .collect();
        for (index, hash) in hashes.iter().enumerate() {
            assert!(!hashes[index + 1..].contains(hash));
        }
    }
}
