--
-- FROM_FUNCTION
-- An ordinary, non-set-returning function called in FROM position: PostgreSQL
-- allows any function there, and one that returns a plain value is a one-row,
-- one-column rowset named after the function. Expected output generated from
-- PostgreSQL 18.4.
--
-- the column is named after the function
SELECT * FROM abs(-1);
-- a qualifier does not change that name
SELECT * FROM pg_catalog.abs(-1);
-- a bare alias renames the scalar column; an alias list wins over it
SELECT * FROM abs(-1) t;
SELECT * FROM abs(-1) AS t;
SELECT * FROM abs(-1) t(v);
-- WITH ORDINALITY joins the rowset like it does any other
SELECT * FROM abs(-1) WITH ORDINALITY;
SELECT * FROM abs(-1) WITH ORDINALITY AS t(v, n);
-- a NULL result is a row, not an empty rowset
SELECT * FROM abs(NULL::int);
-- several function items in one FROM clause
SELECT * FROM lower('AB'), upper('cd');
-- untyped literals are steered by the overload, as in the target list
SELECT * FROM pg_indexam_has_property(403, 'can_order');
-- the argument may read a preceding item: a function FROM item is implicitly
-- LATERAL, so the keyword decides nothing
CREATE TABLE from_function_t (id int);
INSERT INTO from_function_t VALUES (-2), (3);
SELECT * FROM from_function_t, abs(id) a ORDER BY id;
SELECT * FROM from_function_t CROSS JOIN abs(from_function_t.id) a ORDER BY id;
SELECT * FROM from_function_t CROSS JOIN LATERAL abs(id) a ORDER BY id;
-- a filter over the function's own column
SELECT a FROM from_function_t, abs(id) a WHERE a > 2;
-- set-returning items are unaffected by the scalar path
SELECT * FROM from_function_t, generate_series(1, id) g ORDER BY id, g;
-- an aggregate is not a function call PG admits here. Known gap: PG decorates
-- both of the errors below with LINE/caret, and the 42883 with the "No function
-- matches the given name and argument types" hint; the messages themselves match.
SELECT * FROM count(1);
-- and an unknown name is still 42883
SELECT * FROM no_such_fn(1);
DROP TABLE from_function_t;
