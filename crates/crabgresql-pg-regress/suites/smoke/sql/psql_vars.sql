--
-- PSQL_VARS --- \set, \unset, \getenv and :var substitution
--
-- Note the echoed lines below show the *unsubstituted* source, as psql -a
-- always echoes what it read; only the statement sent to the server is
-- expanded.
--
\set greeting hello
SELECT :'greeting' AS plain_literal;
SELECT 1 AS :"greeting";
\set expr 1 + 2
SELECT :expr AS plain_value;

-- values concatenate with no separator, and quotes group without appearing
\set path /tmp '/data' '/onek.data'
SELECT :'path' AS concatenated;

-- an argument may reference an earlier variable, including its quoted form
\set nested 'prefix ' :'path' ' suffix'
SELECT :'nested' AS built_up;

-- quoting rules: embedded quotes double, a backslash forces the E'' form
\set apostrophe 'it''s'
SELECT :'apostrophe' AS doubled_quote;
\set double_quoted 'he"re'
SELECT 1 AS :"double_quoted";
\set backslash 'a\\b'
SELECT :'backslash' AS escape_string;
\set empty ''
SELECT :'empty' AS empty_literal;

-- no substitution inside literals, identifiers, dollar quotes or comments
SELECT ':greeting' AS in_literal;
SELECT $$:greeting$$ AS in_dollar_quote;
SELECT 1 AS "col:greeting";
SELECT 1 AS after_comment; -- :greeting

-- `::` is a cast even when a variable shares the type's name
\set int4 boom
SELECT 1::int4 AS still_a_cast;

-- An undefined variable is left in the text verbatim, so the server sees the
-- colon and rejects it. PostgreSQL words this as `syntax error at or near ":"`;
-- the message below is crabgresql's own parser talking.
\unset greeting
SELECT :'greeting' AS undefined;

-- \getenv leaves the variable unset when the environment has no such entry
\getenv absent CRABGRESQL_NO_SUCH_ENV_VAR
SELECT :'absent' AS from_missing_env;
