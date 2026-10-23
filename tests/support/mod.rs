pub fn legacy_vectors(connection: &rusqlite::Connection) {
    connection.execute_batch("BEGIN;
        DROP TRIGGER vectors_delete_value;
        DROP TRIGGER vectors_update_value;
        CREATE TABLE old_vectors(chunk_id TEXT NOT NULL REFERENCES chunks(id),space TEXT NOT NULL REFERENCES models(id),data BLOB NOT NULL,PRIMARY KEY(chunk_id,space));
        INSERT INTO old_vectors SELECT v.chunk_id,v.space,e.data FROM vectors v JOIN vector_values e ON e.id=v.value_id;
        DROP TABLE vectors;
        DROP TABLE vector_values;
        ALTER TABLE old_vectors RENAME TO vectors;
        PRAGMA user_version=4;
        COMMIT;").unwrap();
}
