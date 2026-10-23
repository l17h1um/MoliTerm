//! SSH key generator (SPEC §5.1.3): ed25519 (default) or RSA-4096, saved as
//! OpenSSH (optionally passphrase-protected) with a `.pub` next to it.
//! RSA generation takes seconds — run off the UI thread.

use std::path::Path;

use russh::keys::{
    Algorithm, PrivateKey,
    ssh_key::{LineEnding, private::RsaKeypair, rand_core::UnwrapErr},
};

use crate::Error;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyType {
    Ed25519,
    Rsa4096,
}

fn crypto(e: impl std::fmt::Display) -> Error {
    Error::Crypto(e.to_string())
}

/// Writes `path` (0600) and `path.pub`; returns the SHA256 fingerprint.
pub fn generate(kind: KeyType, comment: &str, passphrase: &str, path: &Path) -> Result<String, Error> {
    let mut rng = UnwrapErr(getrandom::SysRng);
    let mut key = match kind {
        KeyType::Ed25519 => PrivateKey::random(&mut rng, Algorithm::Ed25519).map_err(crypto)?,
        KeyType::Rsa4096 => PrivateKey::from(RsaKeypair::random(&mut rng, 4096).map_err(crypto)?),
    };
    key.set_comment(comment);
    let public = key.public_key().to_openssh().map_err(crypto)?;
    let fingerprint = crate::known_hosts::fingerprint(key.public_key());
    if !passphrase.is_empty() {
        key = key.encrypt(&mut rng, passphrase).map_err(crypto)?;
    }
    let pem = key.to_openssh(LineEnding::LF).map_err(crypto)?;
    if path.exists() {
        return Err(Error::Crypto(format!("{} already exists", path.display())));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, pem.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let mut pub_path = path.as_os_str().to_owned();
    pub_path.push(".pub");
    std::fs::write(pub_path, format!("{public}\n"))?;
    Ok(fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_roundtrip_with_passphrase() {
        let dir = std::env::temp_dir().join(format!("moliterm-keys-{}", std::process::id()));
        let path = dir.join("id_test");
        let fp = generate(KeyType::Ed25519, "me@test", "pw", &path).unwrap();
        assert!(fp.starts_with("SHA256:"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(matches!(
            russh::keys::decode_secret_key(&text, None),
            Err(russh::keys::Error::KeyIsEncrypted)
        ));
        let key = russh::keys::decode_secret_key(&text, Some("pw")).unwrap();
        assert_eq!(crate::known_hosts::fingerprint(key.public_key()), fp);
        assert!(
            std::fs::read_to_string(dir.join("id_test.pub"))
                .unwrap()
                .starts_with("ssh-ed25519 ")
        );
        assert!(generate(KeyType::Ed25519, "", "", &path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
