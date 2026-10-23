//! Built-in SSH agent served over a forwarded channel (SPEC §5.1.4, §9.3).
//! Minimal protocol: list identities and sign; everything else answers FAILURE.

use russh::keys::{
    HashAlg, PrivateKey,
    signature::Signer,
    ssh_encoding::Encode,
    ssh_key::{Signature, private::KeypairData},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::ssh::AgentKeys;

const FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;
const RSA_SHA2_256: u32 = 2;
const RSA_SHA2_512: u32 = 4;

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

fn take<'a>(buf: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if buf.len() < n {
        return None;
    }
    let (a, b) = buf.split_at(n);
    *buf = b;
    Some(a)
}

fn take_u32(buf: &mut &[u8]) -> Option<u32> {
    take(buf, 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn take_string<'a>(buf: &mut &'a [u8]) -> Option<&'a [u8]> {
    let n = take_u32(buf)? as usize;
    take(buf, n)
}

fn sign(key: &PrivateKey, data: &[u8], flags: u32) -> Option<Signature> {
    match key.key_data() {
        KeypairData::Rsa(rsa) => {
            let hash = if flags & RSA_SHA2_512 != 0 {
                Some(HashAlg::Sha512)
            } else if flags & RSA_SHA2_256 != 0 {
                Some(HashAlg::Sha256)
            } else {
                None
            };
            (rsa, hash).try_sign(data).ok()
        }
        _ => key.try_sign(data).ok(),
    }
}

/// Answers one agent request; `None` = FAILURE.
pub fn answer(keys: &AgentKeys, req: &[u8]) -> Option<Vec<u8>> {
    let keys: Vec<_> = keys.lock().ok()?.clone();
    let (&kind, mut rest) = req.split_first()?;
    match kind {
        REQUEST_IDENTITIES => {
            let mut out = vec![IDENTITIES_ANSWER];
            out.extend_from_slice(&(keys.len() as u32).to_be_bytes());
            for k in &keys {
                put_string(&mut out, &k.public_key().to_bytes().ok()?);
                put_string(&mut out, k.comment().as_bytes());
            }
            Some(out)
        }
        SIGN_REQUEST => {
            let blob = take_string(&mut rest)?;
            let data = take_string(&mut rest)?;
            let flags = take_u32(&mut rest).unwrap_or(0);
            let key = keys
                .iter()
                .find(|k| k.public_key().to_bytes().ok().as_deref() == Some(blob))?;
            let sig = sign(key, data, flags)?;
            let mut out = vec![SIGN_RESPONSE];
            put_string(&mut out, &sig.encode_vec().ok()?);
            Some(out)
        }
        _ => None,
    }
}

pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, keys: AgentKeys) {
    loop {
        let mut len = [0u8; 4];
        if s.read_exact(&mut len).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes(len) as usize;
        if len > 256 * 1024 {
            return;
        }
        let mut req = vec![0u8; len];
        if s.read_exact(&mut req).await.is_err() {
            return;
        }
        let reply = answer(&keys, &req).unwrap_or_else(|| vec![FAILURE]);
        let mut frame = (reply.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&reply);
        if s.write_all(&frame).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{PublicKey, signature::Verifier};
    use std::sync::{Arc, Mutex};

    #[test]
    fn identities_and_sign() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACBES+k5qhJtOxZXfXRSQTyH6bo/Iz/Rfsba+nNWDtJeggAAAJg978fxPe/H
8QAAAAtzc2gtZWQyNTUxOQAAACBES+k5qhJtOxZXfXRSQTyH6bo/Iz/Rfsba+nNWDtJegg
AAAECyDsEaF+jvao70m0FsT7nQpd8GsOzgrl0UVm7QG+x+bkRL6TmqEm07Fld9dFJBPIfp
uj8jP9F+xtr6c1YO0l6CAAAAEXRlc3RAbW9saXRlcm0uZGV2AQIDBA==
-----END OPENSSH PRIVATE KEY-----";
        let key = Arc::new(russh::keys::decode_secret_key(pem, None).unwrap());
        let keys: AgentKeys = Arc::new(Mutex::new(vec![key.clone()]));
        let list = answer(&keys, &[REQUEST_IDENTITIES]).unwrap();
        assert_eq!(list[0], IDENTITIES_ANSWER);
        assert_eq!(&list[1..5], &1u32.to_be_bytes());

        let blob = key.public_key().to_bytes().unwrap();
        let mut req = vec![SIGN_REQUEST];
        put_string(&mut req, &blob);
        put_string(&mut req, b"hello");
        req.extend_from_slice(&0u32.to_be_bytes());
        let resp = answer(&keys, &req).unwrap();
        assert_eq!(resp[0], SIGN_RESPONSE);
        let mut rest = &resp[1..];
        let sig_bytes = take_string(&mut rest).unwrap();
        let sig = Signature::try_from(sig_bytes).unwrap();
        let public: PublicKey = key.public_key().clone();
        Verifier::verify(public.key_data(), b"hello", &sig).unwrap();
        assert!(answer(&keys, &[99]).is_none());
    }
}
