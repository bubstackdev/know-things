BEGIN IMMEDIATE;
CREATE TABLE collection_policies (
 collection_id TEXT PRIMARY KEY REFERENCES collections(id) ON DELETE CASCADE,
 policy TEXT NOT NULL
);
INSERT INTO collection_policies SELECT id,'{"excludedPaths":[],"reconcileIntervalMs":null}' FROM collections;
PRAGMA user_version=3;
COMMIT;
