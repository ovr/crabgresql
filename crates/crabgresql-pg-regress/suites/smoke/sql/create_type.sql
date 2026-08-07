--
-- CREATE_TYPE / CREATE_FUNCTION (internal) / CREATE_CAST / DROP TYPE CASCADE
-- Bootstraps a user-defined base type the way PostgreSQL's own catalog does: a
-- shell type, its internal C I/O functions, the filled-in base type, a binary-
-- coercible cast, then a cascading drop. Output hand-checked against PostgreSQL
-- (psql -a -q), including the argument-shell NOTICE's LINE/caret excerpt (the
-- long CREATE FUNCTION line is truncated to 60 columns with a trailing `...`).
--
-- A bare CREATE TYPE makes a placeholder "shell" type (no NOTICE of its own).
CREATE TYPE xfloat8;
-- The I/O functions reference the shell, so each draws an "only a shell" NOTICE.
CREATE FUNCTION xfloat8in(cstring) RETURNS xfloat8 AS 'int8in' LANGUAGE internal IMMUTABLE STRICT;
CREATE FUNCTION xfloat8out(xfloat8) RETURNS cstring AS 'int8out' LANGUAGE internal IMMUTABLE STRICT;
-- Filling in the shell turns it into a real base type (LIKE int8: 8-byte width).
CREATE TYPE xfloat8 (
   input = xfloat8in,
   output = xfloat8out,
   like = int8
);
-- int8 and xfloat8 share a representation, so the cast needs no function.
CREATE CAST (xfloat8 AS int8) WITHOUT FUNCTION;
-- An unknown internal name is rejected, as in PG.
CREATE FUNCTION bad_in(cstring) RETURNS int8 AS 'no_such_internal_fn' LANGUAGE internal;
-- A binary cast between differently-sized types is refused.
CREATE TYPE xshort;
CREATE FUNCTION xshort_in(cstring) RETURNS xshort AS 'int2in' LANGUAGE internal IMMUTABLE STRICT;
CREATE FUNCTION xshort_out(xshort) RETURNS cstring AS 'int2out' LANGUAGE internal IMMUTABLE STRICT;
CREATE TYPE xshort (input = xshort_in, output = xshort_out, like = int2);
CREATE CAST (xshort AS int8) WITHOUT FUNCTION;
-- Without CASCADE, dropping a type in use is refused and lists the dependents.
DROP TYPE xfloat8;
-- CASCADE drops the type and everything that depends on it, with a NOTICE.
DROP TYPE xfloat8 CASCADE;
DROP TYPE xshort CASCADE;
-- A single dependent is named inline, with no count and no DETAIL.
CREATE TYPE solo;
CREATE FUNCTION solo_in(cstring) RETURNS solo AS 'int8in' LANGUAGE internal IMMUTABLE STRICT;
DROP TYPE solo CASCADE;
-- Re-declaring an existing type is a duplicate-object error.
CREATE TYPE dup;
CREATE TYPE dup;
DROP TYPE dup;
-- A duplicate cast for the same type pair is rejected (so it is not
-- double-counted in a later cascade).
CREATE TYPE xc;
CREATE FUNCTION xc_in(cstring) RETURNS xc AS 'int8in' LANGUAGE internal IMMUTABLE STRICT;
CREATE FUNCTION xc_out(xc) RETURNS cstring AS 'int8out' LANGUAGE internal IMMUTABLE STRICT;
CREATE TYPE xc (input = xc_in, output = xc_out, like = int8);
CREATE CAST (xc AS int8) WITHOUT FUNCTION;
CREATE CAST (xc AS int8) WITHOUT FUNCTION;
DROP TYPE xc CASCADE;
-- An unknown type name in a cast is an undefined-object error.
CREATE CAST (nosuchtype AS int8) WITHOUT FUNCTION;
