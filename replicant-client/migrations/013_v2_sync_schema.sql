-- Protocol v2 local schema. Additive: v1 tables stay until migration 014.

ALTER TABLE documents ADD COLUMN hash TEXT;
ALTER TABLE documents ADD COLUMN server_content TEXT;
ALTER TABLE documents ADD COLUMN server_hash TEXT;
ALTER TABLE documents ADD COLUMN server_seq INTEGER;
ALTER TABLE documents ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
ALTER TABLE documents ADD COLUMN author_id TEXT;
ALTER TABLE documents ADD COLUMN source_doc_id TEXT;
ALTER TABLE documents ADD COLUMN derived_from TEXT;

-- Markers only: what is uploaded is computed from the shadow and content at send time.
CREATE TABLE outbox (
    mutation_id TEXT PRIMARY KEY,
    doc_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('create', 'update', 'delete')),
    created_at INTEGER NOT NULL,
    parked_error TEXT
);
CREATE INDEX idx_outbox_doc ON outbox(doc_id, mutation_id);

CREATE TABLE change_log (
    local_seq INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('upsert', 'delete')),
    origin_instance TEXT NOT NULL,
    origin TEXT NOT NULL CHECK (origin IN ('local', 'server'))
);

CREATE TABLE change_log_readers (
    instance_id TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL
);

CREATE TABLE subscriptions (
    scope TEXT PRIMARY KEY,
    cursor INTEGER NOT NULL DEFAULT 0
);

-- No foreign key to documents: membership outlives a hard-deleted document.
CREATE TABLE doc_scopes (
    doc_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    member INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (doc_id, scope)
);
CREATE INDEX idx_doc_scopes_scope ON doc_scopes(scope, member, seq);

CREATE TABLE tombstones (
    doc_id TEXT PRIMARY KEY,
    server_seq INTEGER NOT NULL,
    deleted_at INTEGER NOT NULL
);

CREATE TABLE recovered (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_id TEXT NOT NULL,
    content TEXT NOT NULL,
    reason TEXT NOT NULL,
    recovered_at INTEGER NOT NULL
);
