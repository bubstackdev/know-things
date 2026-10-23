BEGIN IMMEDIATE;
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO meta VALUES ('generation','0');
CREATE TABLE collections (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, root TEXT NOT NULL UNIQUE);
CREATE TABLE documents (
 id TEXT PRIMARY KEY, collection_id TEXT NOT NULL REFERENCES collections(id), path TEXT NOT NULL,
 active_revision TEXT, eligible INTEGER NOT NULL DEFAULT 1, excluded INTEGER NOT NULL DEFAULT 0,
 desired INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL,
 UNIQUE(collection_id,path)
);
CREATE TABLE revisions (id TEXT PRIMARY KEY, document_id TEXT NOT NULL REFERENCES documents(id), hash TEXT NOT NULL, raw BLOB NOT NULL);
CREATE TABLE chunks (
 row_id INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, revision_id TEXT NOT NULL REFERENCES revisions(id),
 start INTEGER NOT NULL, end INTEGER NOT NULL, heading TEXT NOT NULL, continuation INTEGER NOT NULL,
 text TEXT NOT NULL, processing TEXT NOT NULL
);
CREATE VIRTUAL TABLE lexical USING fts5(text, title, heading, identifiers, tokenize='unicode61');
CREATE TABLE models (id TEXT PRIMARY KEY, manifest TEXT NOT NULL);
CREATE TABLE vectors (chunk_id TEXT NOT NULL REFERENCES chunks(id), space TEXT NOT NULL REFERENCES models(id), data BLOB NOT NULL, PRIMARY KEY(chunk_id,space));
CREATE TABLE jobs (
 id TEXT PRIMARY KEY, document_id TEXT NOT NULL REFERENCES documents(id), desired INTEGER NOT NULL,
 observed_hash TEXT NOT NULL, processing TEXT NOT NULL, status TEXT NOT NULL, error TEXT
);
CREATE INDEX chunks_revision ON chunks(revision_id);
CREATE INDEX documents_collection ON documents(collection_id,eligible);
CREATE INDEX jobs_document ON jobs(document_id);
PRAGMA user_version=1;
COMMIT;
