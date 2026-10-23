//! Identities (SPEC-9 §9.1): a user name plus a password or a private key,
//! reusable by many sessions. Metadata here; secrets in the vault under
//! `identity:<id>:password|key|passphrase`.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{
    Error,
    db::{Db, now_ms},
};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Password,
    Key,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Identity {
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub name: String,
    #[serde(skip)]
    pub sort: i64,
    pub username: String,
    pub kind: Kind,
    /// OpenSSH public key line, for display (empty for passwords).
    pub public_key: String,
}

impl Identity {
    /// Vault id prefix: `<prefix>:password`, `<prefix>:key`, `<prefix>:passphrase`.
    pub fn secret_prefix(&self) -> String {
        prefix(&self.id)
    }
}

pub fn prefix(id: &str) -> String {
    format!("identity:{id}")
}

/// `key_path` value that makes the SSH code read the key from the vault.
pub fn key_ref(id: &str) -> String {
    format!("vault:{}:key", prefix(id))
}

pub fn load(db: &Db) -> Result<Vec<Identity>, Error> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, name, sort, data FROM identities WHERE deleted_at IS NULL ORDER BY sort, name")?;
    let rows = stmt
        .query_map([], |r| {
            let data: String = r.get(3)?;
            let mut i: Identity = serde_json::from_str(&data).unwrap_or_default();
            i.id = r.get(0)?;
            i.name = r.get(1)?;
            i.sort = r.get(2)?;
            Ok(i)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn save(db: &Db, i: &Identity) -> Result<(), Error> {
    db.conn().execute(
        "INSERT INTO identities (id, name, sort, data, created_at, updated_at, device_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET name = ?2, sort = ?3, data = ?4, updated_at = ?5, device_id = ?6",
        params![
            i.id,
            i.name,
            i.sort,
            serde_json::to_string(i)?,
            now_ms(),
            db.device_id()
        ],
    )?;
    Ok(())
}

pub fn delete(db: &Db, id: &str) -> Result<(), Error> {
    db.conn().execute(
        "UPDATE identities SET deleted_at = ?2, updated_at = ?2, device_id = ?3 WHERE id = ?1",
        params![id, now_ms(), db.device_id()],
    )?;
    Ok(())
}

/// Public key line of an OpenSSH/PEM private key (`None` if it can't be read,
/// e.g. encrypted without the passphrase).
pub fn public_key_of(private: &str, passphrase: Option<&str>) -> Option<String> {
    let k = russh::keys::decode_secret_key(private, passphrase).ok()?;
    k.public_key().to_openssh().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let i = Identity {
            id: crate::sessions::new_id(),
            name: "prod-admin".into(),
            username: "admin".into(),
            kind: Kind::Key,
            ..Default::default()
        };
        save(&db, &i).unwrap();
        assert_eq!(load(&db).unwrap(), vec![i.clone()]);
        assert_eq!(key_ref(&i.id), format!("vault:identity:{}:key", i.id));
        delete(&db, &i.id).unwrap();
        assert!(load(&db).unwrap().is_empty());
        assert_eq!(public_key_of("not a key", None), None);
    }
}
