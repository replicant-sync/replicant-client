-- The JSON Pointer the stored titles were derived from: '' for none, NULL until first recorded.
-- An open with a different pointer recomputes every title and the search index.
ALTER TABLE user_config ADD COLUMN title_pointer TEXT;
