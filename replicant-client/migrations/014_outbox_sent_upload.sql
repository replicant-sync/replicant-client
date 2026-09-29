-- The upload a pending row was last sent in. A snapshot rebases pending rows onto itself only
-- when none is marked: a sent but unacknowledged upload may already be applied there.
ALTER TABLE outbox ADD COLUMN sent_upload_id TEXT;
