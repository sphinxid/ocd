//! The tunnel key and certificate request (`client/src/effect/csr.ts` and the key export in `client.ts`).
//!
//! The key is ECDSA P-256, stored as PKCS#8 PEM. The request is PKCS#10 with CN = hostname and the SANs hostname and
//! `*.hostname`, signed with ecdsa-with-SHA256, DER-encoded exactly like the SDK's hand-rolled encoder.

use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

const COMMON_NAME: &str = "2.5.4.3";
const EXTENSION_REQUEST: &str = "1.2.840.113549.1.9.14";
const SUBJECT_ALT_NAME: &str = "2.5.29.17";
const ECDSA_WITH_SHA256: &str = "1.2.840.10045.4.3.2";
const EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
const PRIME256V1: &str = "1.2.840.10045.3.1.7";

pub(crate) fn der(tag: u8, contents: &[&[u8]]) -> Vec<u8> {
    let body: Vec<u8> = contents.concat();
    let mut out = vec![tag];
    if body.len() < 0x80 {
        out.push(body.len() as u8);
    } else {
        let mut bytes = Vec::new();
        let mut n = body.len();
        while n > 0 {
            bytes.insert(0, (n & 0xff) as u8);
            n >>= 8;
        }
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend(body);
    out
}

fn sequence(contents: &[&[u8]]) -> Vec<u8> {
    der(0x30, contents)
}

fn set(contents: &[&[u8]]) -> Vec<u8> {
    der(0x31, contents)
}

pub(crate) fn oid(dotted: &str) -> Vec<u8> {
    let arcs: Vec<u64> = dotted.split('.').map(|a| a.parse().unwrap_or(0)).collect();
    let mut bytes = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &arc in &arcs[2..] {
        let mut encoded = vec![(arc & 0x7f) as u8];
        let mut n = arc >> 7;
        while n > 0 {
            encoded.insert(0, ((n & 0x7f) | 0x80) as u8);
            n >>= 7;
        }
        bytes.extend(encoded);
    }
    der(0x06, &[&bytes])
}

/// PEM with 64-column base64 lines.
fn pem_lines(der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    body.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("\n")
}

/// A new tunnel key: the PKCS#8 PEM (as the SDK writes `private-key.pem`) and the key pair.
pub struct TunnelKey {
    pub pem: String,
    pair: EcdsaKeyPair,
}

impl TunnelKey {
    pub fn generate() -> Result<TunnelKey, String> {
        let rng = SystemRandom::new();
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).map_err(|e| format!("{e:?}"))?;
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .map_err(|e| format!("{e}"))?;
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", pem_lines(pkcs8.as_ref()));
        Ok(TunnelKey { pem, pair })
    }

    /// The SubjectPublicKeyInfo DER (`crypto.subtle.exportKey("spki", ...)`).
    pub fn spki(&self) -> Vec<u8> {
        let algorithm = sequence(&[&oid(EC_PUBLIC_KEY), &oid(PRIME256V1)]);
        let point = self.pair.public_key().as_ref();
        let bits = der(0x03, &[&[0], point]);
        sequence(&[&algorithm, &bits])
    }

    /// `certificateRequest(hostname, keys)`: the PEM request, without a trailing newline.
    pub fn certificate_request(&self, hostname: &str) -> Result<String, String> {
        let utf8 = |v: &str| der(0x0c, &[v.as_bytes()]);
        let dns = |v: &str| der(0x82, &[v.as_bytes()]);
        let wildcard = format!("*.{hostname}");
        let names = sequence(&[&dns(hostname), &dns(&wildcard)]);
        let extension = sequence(&[&oid(SUBJECT_ALT_NAME), &der(0x04, &[&names])]);
        let attribute = sequence(&[&oid(EXTENSION_REQUEST), &set(&[&sequence(&[&extension])])]);
        let subject = sequence(&[&set(&[&sequence(&[&oid(COMMON_NAME), &utf8(hostname)])])]);
        let info = sequence(&[&der(0x02, &[&[0]]), &subject, &self.spki(), &der(0xa0, &[&attribute])]);
        let rng = SystemRandom::new();
        // ring signs as DER SEQUENCE { r INTEGER, s INTEGER }, the minimal encoding the SDK builds from r‖s.
        let signature = self.pair.sign(&rng, &info).map_err(|e| format!("{e}"))?;
        let request =
            sequence(&[&info, &sequence(&[&oid(ECDSA_WITH_SHA256)]), &der(0x03, &[&[0], signature.as_ref()])]);
        let base64 = base64::engine::general_purpose::STANDARD.encode(&request);
        let mut lines = String::new();
        for chunk in base64.as_bytes().chunks(64) {
            lines.push_str(std::str::from_utf8(chunk).unwrap());
            lines.push('\n');
        }
        Ok(format!("-----BEGIN CERTIFICATE REQUEST-----\n{lines}-----END CERTIFICATE REQUEST-----"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_lengths_and_oids() {
        assert_eq!(der(0x04, &[&[1, 2]]), vec![4, 2, 1, 2]);
        let long = vec![0u8; 200];
        assert_eq!(&der(0x04, &[&long])[..3], &[4, 0x81, 200]);
        let longer = vec![0u8; 300];
        assert_eq!(&der(0x04, &[&longer])[..4], &[4, 0x82, 1, 44]);
        assert_eq!(oid("2.5.29.17"), vec![6, 3, 0x55, 0x1d, 0x11]);
        assert_eq!(oid("1.2.840.10045.4.3.2"), vec![6, 8, 0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2]);
    }

    #[test]
    fn key_and_request_shapes() {
        let key = TunnelKey::generate().unwrap();
        assert!(key.pem.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        assert!(key.pem.ends_with("\n-----END PRIVATE KEY-----\n"));
        assert!(key.pem.lines().all(|l| l.len() <= 64));
        assert_eq!(key.spki().len(), 91);
        let csr = key.certificate_request("abc.example").unwrap();
        assert!(csr.starts_with("-----BEGIN CERTIFICATE REQUEST-----\n"));
        assert!(csr.ends_with("\n-----END CERTIFICATE REQUEST-----"));
        // rustls (and Node) load the key.
        use rustls::pki_types::PrivateKeyDer;
        use rustls::pki_types::pem::PemObject;
        let parsed = PrivateKeyDer::from_pem_slice(key.pem.as_bytes()).unwrap();
        assert!(rustls::crypto::ring::sign::any_supported_type(&parsed).is_ok());
    }
}
