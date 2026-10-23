-- Identities (SPEC-9 §9.1): reusable user + secret; secrets live in vault_entries.
CREATE TABLE identities (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    sort       INTEGER NOT NULL DEFAULT 0,
    data       TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    device_id  TEXT NOT NULL
);
