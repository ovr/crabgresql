--
-- ERRORS
-- Error rendering and session recovery: the connection stays usable after
-- every kind of failure.
--
SELECT * FROM missing;
SELEC 1;
SELECT 1 ORDER BY 1;
SELECT 'still alive' AS status;
-- metacommands are not implemented; the runner emits a deterministic stub
\d crabs
SELECT 2 AS after_metacommand;
