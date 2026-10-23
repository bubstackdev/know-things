//! Shared immutable vector values; source ownership stays on chunk mappings.
use crate::{contracts::*, embedding, hash, identity};
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Arc;

pub(crate) fn value_id(space: &str, text_hash: &str, data: &[u8]) -> String {
    identity(&["vector-value-v1", space, text_hash, &hash(data)])
}

pub(crate) fn migrate(connection: &mut Connection) -> Result<()> {
    let tx = connection.transaction()?;
    // Refuse invalid legacy bytes rather than normalizing or replacing evidence.
    crate::integrity::canonical(&tx, None)?;
    tx.execute_batch(include_str!("../migrations/005.sql"))?;
    {
        let mut query = tx.prepare("SELECT v.chunk_id,v.space,c.text,v.data FROM legacy_vectors v JOIN chunks c ON c.id=v.chunk_id ORDER BY v.chunk_id,v.space")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let chunk: String = row.get(0)?;
            let space: String = row.get(1)?;
            let text: String = row.get(2)?;
            let data: Vec<u8> = row.get(3)?;
            put(&tx, &chunk, &space, &hash(text.as_bytes()), &data)?;
        }
    }
    tx.execute_batch("DROP TABLE legacy_vectors; PRAGMA user_version=5;")?;
    crate::integrity::canonical(&tx, None)?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn put(
    connection: &Connection,
    chunk: &str,
    space: &str,
    text_hash: &str,
    data: &[u8],
) -> Result<()> {
    let id = value_id(space, text_hash, data);
    connection.execute(
        "INSERT OR IGNORE INTO vector_values VALUES (?1,?2,?3,?4)",
        params![id, space, text_hash, data],
    )?;
    let agrees: bool = connection.query_row(
        "SELECT space=?2 AND text_hash=?3 AND data=?4 FROM vector_values WHERE id=?1",
        params![id, space, text_hash, data],
        |r| r.get(0),
    )?;
    if !agrees {
        return Err(Error::new(
            "store_corrupt",
            "Shared vector value identity mismatch",
        ));
    }
    connection.execute("INSERT INTO vectors VALUES (?1,?2,?3) ON CONFLICT(chunk_id,space) DO UPDATE SET value_id=excluded.value_id", params![chunk,space,id])?;
    Ok(())
}

pub(crate) fn cached(
    connection: &Connection,
    space: &str,
    text_hash: &str,
    dimensions: usize,
) -> Result<Option<Arc<Vec<u8>>>> {
    let found: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT id,data FROM vector_values WHERE space=?1 AND text_hash=?2 ORDER BY id LIMIT 1",
            params![space, text_hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    found
        .map(|(id, data)| {
            if data.len() != dimensions * 4
                || value_id(space, text_hash, &data) != id
                || (embedding::dot(&data, &data)? - 1.0).abs() > 0.001
            {
                return Err(Error::new(
                    "store_corrupt",
                    "Cached vector value is invalid",
                ));
            }
            Ok(Arc::new(data))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    #[test]
    fn capacity_failure_rolls_back_the_entire_vector_migration_and_can_retry() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(include_str!("../migrations/001.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/002.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/003.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/004.sql"))
            .unwrap();
        let pages: i64 = connection
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        assert_eq!(
            super::migrate(&mut connection).unwrap_err().code,
            "resource_exhausted"
        );
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            4
        );
        assert_eq!(connection.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('vector_values','legacy_vectors','vectors_delete_value','vectors_update_value')",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        connection
            .prepare("SELECT chunk_id,space,data FROM vectors")
            .unwrap();
        crate::integrity::canonical(&connection, None).unwrap();
        connection
            .pragma_update(None, "max_page_count", 1_000_000)
            .unwrap();
        super::migrate(&mut connection).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
        crate::integrity::canonical(&connection, None).unwrap();
    }
}
