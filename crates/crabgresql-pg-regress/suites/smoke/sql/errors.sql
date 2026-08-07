--
-- ERRORS
-- Error rendering and session recovery: the connection stays usable after
-- every kind of failure.
--
SELECT * FROM missing;
SELEC 1;
SELECT 1 FETCH FIRST 1 ROW ONLY;
SELECT 'still alive' AS status;
-- most metacommands are not implemented; the runner emits a deterministic stub
-- rather than guessing (`\d <relation>` is the exception — see psql_describe)
\dt
SELECT 2 AS after_metacommand;
