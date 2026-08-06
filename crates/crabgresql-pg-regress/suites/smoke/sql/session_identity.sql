--
-- SESSION IDENTITY
-- current_database/current_schema(s)/current_user, the temp-namespace
-- predicates, and the encoding number table.
--
-- The identity lines below are literal rather than curated: the regress client
-- connects as user "postgres" to database "regression", exactly as upstream
-- pg_regress does, so PostgreSQL prints the same names. The pg_temp_N *number*
-- is the one session-dependent value here, so those lines compare instead of
-- printing.
--
-- the connection identity, and its keyword aliases
SELECT current_database(), current_catalog;
SELECT current_user, current_role, user, session_user;
-- ...all one value: crabgresql has no SET ROLE, so the three role spellings
-- cannot diverge
SELECT current_user = session_user AS same_role,
       current_database() = current_catalog AS same_db;
-- current_schema is the rare one PostgreSQL spells both ways
SELECT current_schema, current_schema();
-- the search path, with the implicit members and without
SELECT current_schemas(true);
SELECT current_schemas(false);
SELECT current_schemas(NULL) IS NULL AS strict_in_the_flag;
-- the declared types, which are `name` throughout rather than text
SELECT pg_typeof(current_user) AS user_type,
       pg_typeof(current_schema) AS schema_type,
       pg_typeof(current_schemas(true)) AS schemas_type,
       pg_typeof(current_database()) AS db_type;
SELECT pg_typeof(pg_my_temp_schema()) AS temp_type,
       pg_typeof(pg_is_other_temp_schema(0)) AS other_type,
       pg_typeof(pg_char_to_encoding('UTF8')) AS enc_num_type,
       pg_typeof(pg_encoding_to_char(6)) AS enc_name_type;
-- before a temp relation exists there is no temp namespace to name
SELECT pg_my_temp_schema();
SELECT pg_is_other_temp_schema(0), pg_is_other_temp_schema(2200),
       pg_is_other_temp_schema(999999);
-- creating one instantiates it: it heads the implicit path, but never appears
-- in the explicit one
CREATE TEMP TABLE session_identity_t (x int);
SELECT pg_my_temp_schema() <> 0 AS instantiated;
SELECT array_length(current_schemas(true), 1) AS with_implicit,
       current_schemas(false) AS without_implicit;
-- ...and this session's own temp schema is never the "other" one
SELECT pg_is_other_temp_schema(pg_my_temp_schema()) AS not_other;
SELECT pg_is_other_temp_schema(NULL) IS NULL AS strict;
DROP TABLE session_identity_t;
-- the encoding table is numbered, not named: 6 is UTF8 everywhere
SELECT pg_encoding_to_char(6), pg_encoding_to_char(0), pg_encoding_to_char(8),
       pg_encoding_to_char(41);
-- an out-of-range number is the empty string, not NULL
SELECT pg_encoding_to_char(42) = '' AS past_the_end,
       pg_encoding_to_char(999) = '' AS way_past,
       pg_encoding_to_char(-1) = '' AS below,
       pg_encoding_to_char(NULL) IS NULL AS strict;
-- ...and an unknown name is -1, not NULL
SELECT pg_char_to_encoding('nosuch'), pg_char_to_encoding(''),
       pg_char_to_encoding(NULL);
-- names are matched with punctuation and case ignored, and the aliases hold
SELECT pg_char_to_encoding('UTF8'), pg_char_to_encoding('utf8'),
       pg_char_to_encoding('UTF-8'), pg_char_to_encoding('  UTF8 '),
       pg_char_to_encoding('unicode');
SELECT pg_char_to_encoding('latin-1'), pg_char_to_encoding('iso8859_1'),
       pg_char_to_encoding('ISO-8859-5'), pg_char_to_encoding('windows1252'),
       pg_char_to_encoding('mskanji'), pg_char_to_encoding('alt');
-- 'koi' is not an alias for KOI8R, though 'koi8' is
SELECT pg_char_to_encoding('koi8'), pg_char_to_encoding('koi');
-- the two directions are inverses across the whole table
SELECT count(*) AS roundtrips
  FROM generate_series(0, 41) i
 WHERE pg_char_to_encoding(pg_encoding_to_char(i)) = i;
