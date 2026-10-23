use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};
use std::path::Path;

use crate::Error;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_sessions_vault.sql"),
    include_str!("../migrations/0003_tunnels.sql"),
    include_str!("../migrations/0004_macros.sql"),
    include_str!("../migrations/0005_identities.sql"),
];

pub struct Db {
    conn: Connection,
    device_id: String,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let db = Self::init(Connection::open(path)?)?;
        // The DB holds the vault ciphertext (SPEC §9.2: 0600 on Unix).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self, Error> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, Error> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        let device_id = match conn
            .query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| r.get(0))
            .optional()?
        {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::now_v7().to_string();
                conn.execute("INSERT INTO meta (key, value) VALUES ('device_id', ?1)", [&id])?;
                id
            }
        };
        Ok(Self { conn, device_id })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Device-local values (not synced): window layout, restore state.
    pub fn meta_get(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
            [key, value],
        )?;
        Ok(())
    }

    /// All stored setting rows as one JSON object.
    pub fn settings_map(&self) -> Result<Map<String, Value>, Error> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut map = Map::new();
        for row in rows {
            let (key, value) = row?;
            if let Ok(v) = serde_json::from_str(&value) {
                map.insert(key, v);
            }
        }
        Ok(map)
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO settings (key, value, updated_at, device_id) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3, device_id = ?4",
            params![key, value.to_string(), now_ms(), self.device_id],
        )?;
        Ok(())
    }
}

fn migrate(conn: &mut Connection) -> Result<(), Error> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in (1..).zip(MIGRATIONS).skip(version.max(0) as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i)?;
        tx.commit()?;
    }
    Ok(())
}

/// UTC milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_and_device_id() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.device_id().len(), 36);
        db.set_setting("theme", &Value::from("Monokai")).unwrap();
        db.set_setting("theme", &Value::from("Ayu Light")).unwrap();
        assert_eq!(db.settings_map().unwrap()["theme"], "Ayu Light");
    }
}
