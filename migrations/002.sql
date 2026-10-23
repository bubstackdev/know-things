BEGIN IMMEDIATE;
ALTER TABLE documents ADD COLUMN active_processing TEXT;
UPDATE documents SET active_processing='markdown-blocks-v1-max4096bytes' WHERE active_revision IS NOT NULL;
PRAGMA user_version=2;
COMMIT;
