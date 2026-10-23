-- Executed inside the Rust migration transaction; values are populated with
-- versioned identities before the legacy table is dropped and version advances.
ALTER TABLE vectors RENAME TO legacy_vectors;
CREATE TABLE vector_values (
 id TEXT PRIMARY KEY,
 space TEXT NOT NULL REFERENCES models(id),
 text_hash TEXT NOT NULL,
 data BLOB NOT NULL,
 UNIQUE(id,space)
);
CREATE INDEX vector_values_input ON vector_values(space,text_hash,id);
CREATE TABLE vectors (
 chunk_id TEXT NOT NULL REFERENCES chunks(id),
 space TEXT NOT NULL REFERENCES models(id),
 value_id TEXT NOT NULL,
 PRIMARY KEY(chunk_id,space),
 FOREIGN KEY(value_id,space) REFERENCES vector_values(id,space)
);
CREATE INDEX vectors_value ON vectors(value_id);
CREATE TRIGGER vectors_delete_value AFTER DELETE ON vectors BEGIN
 DELETE FROM vector_values WHERE id=old.value_id
  AND NOT EXISTS(SELECT 1 FROM vectors WHERE value_id=old.value_id);
END;
CREATE TRIGGER vectors_update_value AFTER UPDATE OF value_id ON vectors
 WHEN old.value_id!=new.value_id BEGIN
 DELETE FROM vector_values WHERE id=old.value_id
  AND NOT EXISTS(SELECT 1 FROM vectors WHERE value_id=old.value_id);
END;
