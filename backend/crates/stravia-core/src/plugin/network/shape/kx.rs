use aws_lc_rs::{agreement, kem};
use rustls::crypto::{ActiveKeyExchange, CompletedKeyExchange, SharedSecret, SupportedKxGroup};
use rustls::ffdhe_groups::FfdheGroup;
use rustls::{Error, NamedGroup, PeerMisbehaved, ProtocolVersion};

// RFC 10024 sections 4.1–4.3: P-384 precedes ML-KEM in both shares and the shared secret.
const P384_LEN: usize = 97;
const SHARE_LEN: usize = P384_LEN + 1568;
const GROUP: NamedGroup = NamedGroup::Unknown(0x11ed);
const INVALID_SHARE: Error = Error::PeerMisbehaved(PeerMisbehaved::InvalidKeyShare);

pub(super) static SECP384R1MLKEM1024: &dyn SupportedKxGroup = &Secp384r1MlKem1024;

#[derive(Debug)]
struct Secp384r1MlKem1024;

fn internal_error(operation: &str) -> Error {
    Error::General(format!("SecP384r1MLKEM1024 {operation} failed"))
}

fn split_share(share: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    if share.len() != SHARE_LEN || share[0] != 4 {
        return Err(INVALID_SHARE);
    }
    Ok(share.split_at(P384_LEN))
}

fn p384_key() -> Result<(agreement::PrivateKey, agreement::PublicKey), Error> {
    let private = agreement::PrivateKey::generate(&agreement::ECDH_P384)
        .map_err(|_| internal_error("P-384 key generation"))?;
    let public = private
        .compute_public_key()
        .map_err(|_| internal_error("P-384 public key generation"))?;
    Ok((private, public))
}

fn combine(private: &agreement::PrivateKey, peer: &[u8], pq: &[u8]) -> Result<SharedSecret, Error> {
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P384, peer);
    agreement::agree(private, peer, INVALID_SHARE, |classical| {
        let mut combined = Vec::with_capacity(classical.len() + pq.len());
        combined.extend_from_slice(classical);
        combined.extend_from_slice(pq);
        Ok(SharedSecret::from(combined))
    })
}

impl SupportedKxGroup for Secp384r1MlKem1024 {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let (private, public) = p384_key()?;
        let decapsulation_key = kem::DecapsulationKey::generate(&kem::ML_KEM_1024)
            .map_err(|_| internal_error("ML-KEM key generation"))?;
        let encapsulation_key = decapsulation_key
            .encapsulation_key()
            .and_then(|key| key.key_bytes())
            .map_err(|_| internal_error("ML-KEM public key generation"))?;
        let mut share = Vec::with_capacity(SHARE_LEN);
        share.extend_from_slice(public.as_ref());
        share.extend_from_slice(encapsulation_key.as_ref());
        Ok(Box::new(Active {
            private,
            decapsulation_key,
            share,
        }))
    }

    fn start_and_complete(&self, client_share: &[u8]) -> Result<CompletedKeyExchange, Error> {
        let (classical, pq) = split_share(client_share)?;
        let encapsulation_key =
            kem::EncapsulationKey::new(&kem::ML_KEM_1024, pq).map_err(|_| INVALID_SHARE)?;
        let (ciphertext, pq_secret) = encapsulation_key.encapsulate().map_err(|_| INVALID_SHARE)?;
        let (private, public) = p384_key()?;
        let secret = combine(&private, classical, pq_secret.as_ref())?;
        let mut share = Vec::with_capacity(SHARE_LEN);
        share.extend_from_slice(public.as_ref());
        share.extend_from_slice(ciphertext.as_ref());
        Ok(CompletedKeyExchange {
            group: GROUP,
            pub_key: share,
            secret,
        })
    }

    fn name(&self) -> NamedGroup {
        GROUP
    }
    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }
    fn usable_for_version(&self, version: ProtocolVersion) -> bool {
        version == ProtocolVersion::TLSv1_3
    }
}

struct Active {
    private: agreement::PrivateKey,
    decapsulation_key: kem::DecapsulationKey,
    share: Vec<u8>,
}

impl ActiveKeyExchange for Active {
    fn complete(self: Box<Self>, server_share: &[u8]) -> Result<SharedSecret, Error> {
        let (classical, ciphertext) = split_share(server_share)?;
        let pq_secret = self
            .decapsulation_key
            .decapsulate(ciphertext.into())
            .map_err(|_| Error::MlKemDecapsulationFailed)?;
        combine(&self.private, classical, pq_secret.as_ref())
    }

    fn pub_key(&self) -> &[u8] {
        &self.share
    }
    fn group(&self) -> NamedGroup {
        GROUP
    }
    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, sync::Arc};

    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
    use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

    use super::*;

    #[test]
    fn hybrid_group_completes_a_real_tls13_handshake() {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let mut provider = rustls::crypto::aws_lc_rs::default_provider();
        provider.kx_groups = vec![SECP384R1MLKEM1024];
        let provider = Arc::new(provider);
        let client_config = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
            )
            .unwrap();
        let mut client = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
        while client.is_handshaking() || server.is_handshaking() {
            let mut wire = Vec::new();
            client.write_tls(&mut wire).unwrap();
            server.read_tls(&mut Cursor::new(wire)).unwrap();
            server.process_new_packets().unwrap();
            let mut wire = Vec::new();
            server.write_tls(&mut wire).unwrap();
            client.read_tls(&mut Cursor::new(wire)).unwrap();
            client.process_new_packets().unwrap();
        }
        assert_eq!(
            client.negotiated_key_exchange_group().unwrap().name(),
            rustls::NamedGroup::Unknown(0x11ed)
        );
        assert_eq!(
            server.negotiated_key_exchange_group().unwrap().name(),
            rustls::NamedGroup::Unknown(0x11ed)
        );
    }

    #[test]
    fn server_share_and_secret_follow_rfc10024() {
        use aws_lc_rs::{agreement, kem};
        let private = agreement::PrivateKey::generate(&agreement::ECDH_P384).unwrap();
        let public = private.compute_public_key().unwrap();
        let decapsulation = kem::DecapsulationKey::generate(&kem::ML_KEM_1024).unwrap();
        let encapsulation = decapsulation
            .encapsulation_key()
            .unwrap()
            .key_bytes()
            .unwrap();
        let client_share = [public.as_ref(), encapsulation.as_ref()].concat();
        let server = SECP384R1MLKEM1024
            .start_and_complete(&client_share)
            .unwrap();
        assert_eq!(server.pub_key.len(), 1665);
        assert_eq!(server.pub_key[0], 4);
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P384, &server.pub_key[..97]);
        let classical = agreement::agree(&private, peer, (), |bytes| Ok(bytes.to_vec())).unwrap();
        let pq = decapsulation
            .decapsulate(server.pub_key[97..].into())
            .unwrap();
        assert_eq!(
            server.secret.secret_bytes(),
            [classical.as_slice(), pq.as_ref()].concat()
        );
        let mut invalid = client_share;
        invalid[97..].fill(0xff);
        assert!(SECP384R1MLKEM1024.start_and_complete(&invalid).is_err());
    }

    #[test]
    fn hybrid_exchange_uses_rfc10024_component_order_and_rejects_invalid_shares() {
        use aws_lc_rs::{agreement, kem};
        let client = SECP384R1MLKEM1024.start().unwrap();
        assert_eq!(client.pub_key().len(), 1665);
        assert_eq!(client.pub_key()[0], 4);
        let ecdh = agreement::PrivateKey::generate(&agreement::ECDH_P384).unwrap();
        let public = ecdh.compute_public_key().unwrap();
        let peer =
            agreement::UnparsedPublicKey::new(&agreement::ECDH_P384, &client.pub_key()[..97]);
        let classical = agreement::agree(&ecdh, peer, (), |bytes| Ok(bytes.to_vec())).unwrap();
        let encaps =
            kem::EncapsulationKey::new(&kem::ML_KEM_1024, &client.pub_key()[97..]).unwrap();
        let (ciphertext, pq) = encaps.encapsulate().unwrap();
        let server_share = [public.as_ref(), ciphertext.as_ref()].concat();
        let expected = [classical.as_slice(), pq.as_ref()].concat();
        assert_eq!(expected.len(), 80);
        assert_eq!(
            client.complete(&server_share).unwrap().secret_bytes(),
            expected
        );
        assert!(
            SECP384R1MLKEM1024
                .start()
                .unwrap()
                .complete(&server_share[..1664])
                .is_err()
        );
        assert!(
            SECP384R1MLKEM1024
                .start_and_complete(&server_share[..1664])
                .is_err()
        );
        let mut invalid = server_share;
        invalid[..97].fill(0);
        assert!(
            SECP384R1MLKEM1024
                .start()
                .unwrap()
                .complete(&invalid)
                .is_err()
        );
        assert!(SECP384R1MLKEM1024.start_and_complete(&invalid).is_err());
    }
}
