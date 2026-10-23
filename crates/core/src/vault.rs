//! Secret vault (SPEC §9.2): master password → Argon2id → 256-bit key; each
//! secret sealed with XChaCha20-Poly1305 (random 24-byte nonce, AD = secret id).
//! Only ciphertext is stored (table `vault_entries`, header in `meta`).
//! Key derivation is slow on purpose — call `create`/`unlock` off the UI thread.

use argon2::{Algorithm, Argon2, Params, Version};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use rand_core::{OsRng, RngCore};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    Error,
    db::{Db, now_ms},
};

const CHECK_ID: &str = "vault-check";
const CHECK_TEXT: &[u8] = b"moliterm-vault-v1";

#[derive(Serialize, Deserialize)]
struct Header {
    salt: String,
    m_kib: u32,
    t: u32,
    p: u32,
    check_nonce: String,
    check: String,
}

/// m = 64 MiB, p = 1; t tuned so unlock takes ~0.5 s on the dev MacBook (DECISIONS.md).
#[cfg(not(test))]
const PARAMS: (u32, u32, u32) = (64 * 1024, 3, 1);
#[cfg(test)]
const PARAMS: (u32, u32, u32) = (64, 1, 1);

pub type Key = Zeroizing<[u8; 32]>;

fn derive(password: &str, salt: &[u8], (m, t, p): (u32, u32, u32)) -> Result<Key, Error> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| Error::Crypto(e.to_string()))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut *key)
        .map_err(|e| Error::Crypto(e.to_string()))?;
    Ok(key)
}

fn seal(key: &Key, id: &str, plain: &[u8]) -> Result<([u8; 24], Vec<u8>), Error> {
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let ct = XChaCha20Poly1305::new(key.as_ref().into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: id.as_bytes(),
            },
        )
        .map_err(|_| Error::Crypto("encrypt".into()))?;
    Ok((nonce, ct))
}

fn open(key: &Key, id: &str, nonce: &[u8], ct: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    if nonce.len() != 24 {
        return Err(Error::Crypto("bad nonce".into()));
    }
    XChaCha20Poly1305::new(key.as_ref().into())
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: id.as_bytes(),
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| Error::WrongPassword)
}

fn header(db: &Db) -> Result<Option<Header>, Error> {
    let json: Option<String> = db
        .conn()
        .query_row("SELECT value FROM meta WHERE key = 'vault'", [], |r| r.get(0))
        .optional()?;
    Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
}

fn write_header(db: &Db, key: &Key, params: (u32, u32, u32), salt: &[u8]) -> Result<(), Error> {
    let (nonce, check) = seal(key, CHECK_ID, CHECK_TEXT)?;
    let h = Header {
        salt: B64.encode(salt),
        m_kib: params.0,
        t: params.1,
        p: params.2,
        check_nonce: B64.encode(nonce),
        check: B64.encode(check),
    };
    db.conn().execute(
        "INSERT INTO meta (key, value) VALUES ('vault', ?1) ON CONFLICT(key) DO UPDATE SET value = ?1",
        [serde_json::to_string(&h)?],
    )?;
    Ok(())
}

pub fn exists(db: &Db) -> bool {
    header(db).ok().flatten().is_some()
}

/// Derives a key for a new vault (slow). Store it with [`Vault::create`].
pub fn new_key(password: &str) -> Result<(Key, [u8; 16]), Error> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    Ok((derive(password, &salt, PARAMS)?, salt))
}

/// Everything needed to check a master password, detached from the DB so the
/// slow derivation can run on another thread.
pub struct Unlocker(Header);

impl Unlocker {
    pub fn load(db: &Db) -> Result<Self, Error> {
        Ok(Self(header(db)?.ok_or(Error::Locked)?))
    }

    /// Derives the key and checks it against the verifier (slow).
    pub fn unlock(&self, password: &str) -> Result<Key, Error> {
        let h = &self.0;
        let b = |s: &str| B64.decode(s).map_err(|e| Error::Crypto(e.to_string()));
        let key = derive(password, &b(&h.salt)?, (h.m_kib, h.t, h.p))?;
        open(&key, CHECK_ID, &b(&h.check_nonce)?, &b(&h.check)?)?;
        Ok(key)
    }
}

pub fn unlock_key(db: &Db, password: &str) -> Result<Key, Error> {
    Unlocker::load(db)?.unlock(password)
}

/// Unlocked or locked vault state held by the app.
#[derive(Default)]
pub struct Vault {
    key: Option<Key>,
}

impl Vault {
    pub fn create(db: &Db, key: Key, salt: &[u8]) -> Result<Self, Error> {
        if exists(db) {
            return Err(Error::Crypto("vault already exists".into()));
        }
        write_header(db, &key, PARAMS, salt)?;
        Ok(Self { key: Some(key) })
    }

    pub fn unlocked(key: Key) -> Self {
        Self { key: Some(key) }
    }

    pub fn is_unlocked(&self) -> bool {
        self.key.is_some()
    }

    /// Drops the key (zeroized on drop).
    pub fn lock(&mut self) {
        self.key = None;
    }

    fn key(&self) -> Result<&Key, Error> {
        self.key.as_ref().ok_or(Error::Locked)
    }

    pub fn get(&self, db: &Db, id: &str) -> Result<Option<Zeroizing<String>>, Error> {
        let key = self.key()?;
        let row: Option<(Vec<u8>, Vec<u8>)> = db
            .conn()
            .query_row(
                "SELECT nonce, ciphertext FROM vault_entries WHERE id = ?1 AND deleted_at IS NULL",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((nonce, ct)) = row else {
            return Ok(None);
        };
        let plain = open(key, id, &nonce, &ct)?;
        Ok(Some(Zeroizing::new(String::from_utf8_lossy(&plain).into_owned())))
    }

    pub fn put(&self, db: &Db, id: &str, secret: &str) -> Result<(), Error> {
        let (nonce, ct) = seal(self.key()?, id, secret.as_bytes())?;
        let now = now_ms();
        db.conn().execute(
            "INSERT INTO vault_entries (id, nonce, ciphertext, created_at, updated_at, deleted_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?4, NULL, ?5)
             ON CONFLICT(id) DO UPDATE SET nonce = ?2, ciphertext = ?3, updated_at = ?4, deleted_at = NULL, device_id = ?5",
            params![id, nonce.as_slice(), ct, now, db.device_id()],
        )?;
        Ok(())
    }

    /// Tombstone (sync-ready); the ciphertext is wiped.
    pub fn delete(&self, db: &Db, id: &str) -> Result<(), Error> {
        db.conn().execute(
            "UPDATE vault_entries SET nonce = x'', ciphertext = x'', deleted_at = ?2, updated_at = ?2, device_id = ?3
             WHERE id = ?1",
            params![id, now_ms(), db.device_id()],
        )?;
        Ok(())
    }

    /// Ids of all live entries (ids are not secret; values are).
    pub fn ids(db: &Db) -> Vec<String> {
        let Ok(mut stmt) = db
            .conn()
            .prepare("SELECT id FROM vault_entries WHERE deleted_at IS NULL ORDER BY id")
        else {
            return vec![];
        };
        stmt.query_map([], |r| r.get(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    pub fn count(db: &Db) -> usize {
        db.conn()
            .query_row("SELECT COUNT(*) FROM vault_entries WHERE deleted_at IS NULL", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_or(0, |n| n as usize)
    }

    /// Re-encrypts every entry under `new_key` in one transaction.
    pub fn change_key(&mut self, db: &Db, new_key: Key, salt: &[u8]) -> Result<(), Error> {
        let old = self.key()?.clone();
        let rows: Vec<(String, Vec<u8>, Vec<u8>)> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT id, nonce, ciphertext FROM vault_entries WHERE deleted_at IS NULL")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?
        };
        db.conn().execute_batch("BEGIN")?;
        let result = (|| {
            for (id, nonce, ct) in rows {
                let plain = open(&old, &id, &nonce, &ct)?;
                let (n, c) = seal(&new_key, &id, &plain)?;
                db.conn().execute(
                    "UPDATE vault_entries SET nonce = ?2, ciphertext = ?3, updated_at = ?4, device_id = ?5 WHERE id = ?1",
                    params![id, n.as_slice(), c, now_ms(), db.device_id()],
                )?;
            }
            write_header(db, &new_key, PARAMS, salt)
        })();
        match result {
            Ok(()) => {
                db.conn().execute_batch("COMMIT")?;
                self.key = Some(new_key);
                Ok(())
            }
            Err(e) => {
                let _ = db.conn().execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_unlock_lock_change() {
        let db = Db::open_in_memory().unwrap();
        assert!(!exists(&db));
        let (key, salt) = new_key("correct horse").unwrap();
        let mut v = Vault::create(&db, key, &salt).unwrap();
        v.put(&db, "session:1:password", "s3cret").unwrap();
        assert_eq!(v.get(&db, "session:1:password").unwrap().unwrap().as_str(), "s3cret");
        assert_eq!(Vault::count(&db), 1);

        v.lock();
        assert!(matches!(v.get(&db, "session:1:password"), Err(Error::Locked)));
        assert!(matches!(unlock_key(&db, "wrong"), Err(Error::WrongPassword)));
        let mut v = Vault::unlocked(unlock_key(&db, "correct horse").unwrap());
        assert_eq!(v.get(&db, "session:1:password").unwrap().unwrap().as_str(), "s3cret");

        let (k2, s2) = new_key("battery staple").unwrap();
        v.change_key(&db, k2, &s2).unwrap();
        assert!(unlock_key(&db, "correct horse").is_err());
        let v = Vault::unlocked(unlock_key(&db, "battery staple").unwrap());
        assert_eq!(v.get(&db, "session:1:password").unwrap().unwrap().as_str(), "s3cret");

        // Ciphertext is bound to its id.
        db.conn()
            .execute(
                "UPDATE vault_entries SET id = 'other' WHERE id = 'session:1:password'",
                [],
            )
            .unwrap();
        assert!(v.get(&db, "other").is_err());
        v.delete(&db, "other").unwrap();
        assert_eq!(Vault::count(&db), 0);
    }
}
