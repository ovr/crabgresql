--
-- VARIADIC parameters and VARIADIC arguments
-- A routine's trailing parameter may stand for any number of arguments, and a
-- call may hand that parameter a whole array instead by writing VARIADIC. The
-- two shapes are exclusive: the spread form does not accept an array, and the
-- VARIADIC form does not spread. Output generated with PostgreSQL (psql -a -q).
-- (The resolution failures — too few arguments, a wrongly-typed array, VARIADIC
-- in a COALESCE-style expression list — are covered by the server e2e tests
-- instead: PG prints a `LINE`/caret excerpt and the "No function matches" HINT
-- on a 42883 that no error this engine raises carries yet.)
--

--
-- Declaring one
--
CREATE FUNCTION vsum(a int, VARIADIC rest int[]) RETURNS int LANGUAGE SQL IMMUTABLE
  AS $$ SELECT a + coalesce(cardinality(rest), 0) $$;
-- The spread form: every trailing argument becomes one array element.
SELECT vsum(1, 2, 3, 4);
SELECT vsum(1, 2);
-- Trailing arguments coerce to the element type by the ordinary implicit rules.
SELECT vsum(1, 2::smallint, 3);

--
-- Calling one with VARIADIC
--
SELECT vsum(1, VARIADIC ARRAY[2, 3]);
SELECT vsum(1, VARIADIC '{2,3}');
-- A NULL array reaches the body as a NULL argument, not as no arguments.
SELECT vsum(1, VARIADIC NULL::int[]);
SELECT vsum(1, VARIADIC ARRAY[]::int[]);

--
-- The declaration's own rules
--
CREATE FUNCTION vbad(VARIADIC a int) RETURNS int LANGUAGE SQL AS $$ SELECT 1 $$;
CREATE FUNCTION vbad(VARIADIC a int[], b int) RETURNS int LANGUAGE SQL AS $$ SELECT 1 $$;
CREATE FUNCTION vbad(VARIADIC a int[], VARIADIC b int[]) RETURNS int LANGUAGE SQL AS $$ SELECT 1 $$;
-- An OUT parameter may still follow: only *inputs* have to stop at the
-- variadic one.
CREATE FUNCTION vout(VARIADIC a int[], OUT b int) RETURNS int LANGUAGE SQL AS $$ SELECT 1 $$;
SELECT pg_get_function_arguments(oid) FROM pg_proc WHERE proname = 'vout';

--
-- The catalog
--
SELECT provariadic, pronargs, proargmodes, proallargtypes, proargnames
  FROM pg_proc WHERE proname = 'vsum';
SELECT proargtypes FROM pg_proc WHERE proname = 'vsum';
SELECT pg_get_function_arguments(oid), pg_get_function_identity_arguments(oid)
  FROM pg_proc WHERE proname = 'vsum';
-- The routine's identity is its declared types, the array among them: VARIADIC
-- is not part of it, and DROP accepts the signature written either way.
SELECT oid::regprocedure FROM pg_proc WHERE proname = 'vsum';

--
-- A PL/pgSQL body receives the array
--
CREATE FUNCTION vjoin(VARIADIC parts text[]) RETURNS text LANGUAGE plpgsql IMMUTABLE
  AS $$ BEGIN RETURN array_to_string(parts, '-'); END $$;
SELECT vjoin('a', 'b', 'c');
SELECT vjoin(VARIADIC ARRAY['x', 'y']);

--
-- The built-in VARIADIC "any" functions
--
SELECT concat(VARIADIC ARRAY['a', 'b', 'c']);
SELECT concat(VARIADIC ARRAY[1, 2, 3]);
SELECT concat(VARIADIC '{}'::int[]) = '';
-- A NULL array is NULL, where a NULL argument would have been skipped.
SELECT concat(VARIADIC NULL::int[]) IS NULL, concat(NULL) = '';
SELECT concat_ws(',', VARIADIC ARRAY[1, 2, 3]);
SELECT concat_ws(',', VARIADIC ARRAY[NULL, 'a', NULL, 'b']::text[]);
SELECT concat_ws(',', VARIADIC NULL::text[]) IS NULL;
SELECT format('%s|%s', VARIADIC ARRAY['a', 'b']);
-- Extra elements past the picture's placeholders are ignored.
SELECT format('%s|%s', VARIADIC ARRAY['a', 'b', 'c']);
-- A NULL array is no arguments at all here, which is too few for the picture.
SELECT format('%s', VARIADIC NULL::text[]);
-- The argument has to be an array: this is the one call shape that says so.
SELECT concat(VARIADIC 10);
SELECT concat_ws(',', VARIADIC 10);

--
-- VARIADIC on a callee that has no variadic parameter is just its array type
--
SELECT cardinality(VARIADIC ARRAY[1, 2]);
SELECT count(VARIADIC ARRAY[1, 2]);

--
-- Grammar: VARIADIC marks the last argument, and nothing may follow it
--
SELECT concat(VARIADIC ARRAY[1], 2);

DROP FUNCTION vjoin(text[]);
DROP FUNCTION vsum(int, VARIADIC int[]);
