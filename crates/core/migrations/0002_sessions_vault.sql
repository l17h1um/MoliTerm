-- Sync-ready rows (SPEC §9.4): UUIDv7 id, timestamps in UTC ms, tombstone, last writer.
-- Everything except tree structure lives in `data` (JSON) so new fields need no migration.
CREATE TABLE folders (
    id         TEXT PRIMARY KEY,
    parent_id  TEXT,
    name       TEXT NOT NULL,
    sort       INTEGER NOT NULL DEFAULT 0,
    data       TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    device_id  TEXT NOT NULL
);

CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    folder_id  TEXT,
    name       TEXT NOT NULL,
    sort       INTEGER NOT NULL DEFAULT 0,
    data       TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    device_id  TEXT NOT NULL
);

-- Vault (SPEC §9.2): only ciphertext. Header row 'vault' in meta holds salt,
-- KDF params and a verifier.
CREATE TABLE vault_entries (
    id         TEXT PRIMARY KEY,
    nonce      BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    device_id  TEXT NOT NULL
);
