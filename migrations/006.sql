-- Policy v3 enables bounded native watch hints, defaulting off for old records.
BEGIN IMMEDIATE;
PRAGMA user_version=6;
COMMIT;
