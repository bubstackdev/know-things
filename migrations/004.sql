-- Collection policy v2 permits persisted retention budgets. Older binaries
-- reject this store before interpreting the new policy JSON contract.
BEGIN IMMEDIATE;
PRAGMA user_version=4;
COMMIT;
