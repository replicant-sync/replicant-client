-- Make user_id nullable to support public documents (no owner)
-- SQLite doesn't support ALTER COLUMN, so we recreate the table

CREATE TABLE documents_new (
    id TEXT PRIMARY KEY,
    user_id TEXT,
    content JSON NOT NULL,
    sync_revision INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP,
    local_changes JSON,
    sync_status TEXT DEFAULT 'synced',
    title TEXT,
    CHECK (sync_status IN ('synced', 'pending', 'conflict'))
);

INSERT INTO documents_new (id, user_id, content, sync_revision, created_at, updated_at, deleted_at, local_changes, sync_status, title)
SELECT id, user_id, content, sync_revision, created_at, updated_at, deleted_at, local_changes, sync_status, title
FROM documents;

DROP TABLE documents;
ALTER TABLE documents_new RENAME TO documents;

CREATE INDEX idx_documents_user_id ON documents(user_id);
CREATE INDEX idx_documents_sync_status ON documents(sync_status);
