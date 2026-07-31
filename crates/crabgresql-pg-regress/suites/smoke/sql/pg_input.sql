--
-- pg_input_is_valid / pg_input_error_info: the non-throwing input path.
-- The type argument is a written type name, so a modifier travels with it and
-- is applied with *input* semantics (over-long errors) rather than the
-- explicit-cast semantics that would truncate.
--

-- boolean
SELECT pg_input_is_valid('true', 'bool') AS ok, pg_input_is_valid('junk', 'bool') AS bad;
SELECT * FROM pg_input_error_info('junk', 'bool');

-- char(n): trailing blanks are absorbed, real characters are not
SELECT pg_input_is_valid('abcd  ', 'char(4)') AS ok, pg_input_is_valid('abcde', 'char(4)') AS bad;
SELECT * FROM pg_input_error_info('abcde', 'char(4)');

-- varchar(n): same rule, different type name in the message
SELECT pg_input_is_valid('abcd  ', 'varchar(4)') AS ok, pg_input_is_valid('abcde', 'varchar(4)') AS bad;
SELECT * FROM pg_input_error_info('abcde', 'varchar(4)');

-- int2: malformed (22P02) and out-of-range (22003) are distinct
SELECT pg_input_is_valid('34', 'int2') AS ok, pg_input_is_valid('asdf', 'int2') AS bad,
       pg_input_is_valid('50000', 'int2') AS too_big;
SELECT * FROM pg_input_error_info('asdf', 'int2');
SELECT * FROM pg_input_error_info('50000', 'int2');

-- oid, likewise
SELECT pg_input_is_valid('1234', 'oid') AS ok, pg_input_is_valid('01XYZ', 'oid') AS bad,
       pg_input_is_valid('9999999999', 'oid') AS too_big;
SELECT * FROM pg_input_error_info('01XYZ', 'oid');
SELECT * FROM pg_input_error_info('9999999999', 'oid');

-- text and name accept every string; name truncates rather than failing
SELECT pg_input_is_valid('anything at all', 'text') AS text_ok,
       pg_input_is_valid(repeat('x', 200), 'name') AS name_ok;

-- numeric carries its typmod too
SELECT pg_input_is_valid('123.45', 'numeric(5,2)') AS ok,
       pg_input_is_valid('123456', 'numeric(5,2)') AS bad;

-- a failure that has a DETAIL line reports it
SELECT * FROM pg_input_error_info('1.2.3.4/24', 'cidr');

-- a schema-qualified built-in resolves like the bare spelling
SELECT pg_input_is_valid('42', 'pg_catalog.int4') AS ok,
       pg_input_is_valid('x', 'pg_catalog.int4') AS bad;

-- an alias or qualified spelling carries its modifier just like char(4)
SELECT pg_input_is_valid('abcd  ', 'bpchar(4)') AS ok, pg_input_is_valid('abcde', 'bpchar(4)') AS bad;
SELECT * FROM pg_input_error_info('abcde', 'pg_catalog.varchar(4)');

-- an array applies its element modifier to every element
SELECT pg_input_is_valid('{abcd,ab}', 'varchar(4)[]') AS ok,
       pg_input_is_valid('{abcde}', 'varchar(4)[]') AS bad;
SELECT * FROM pg_input_error_info('{abcde}', 'varchar(4)[]');

-- a reg* target fails softly when the object is missing: that is a value
-- answer, not an error
SELECT pg_input_is_valid('pg_class', 'regclass') AS ok,
       pg_input_is_valid('no_such_table', 'regclass') AS bad;
SELECT * FROM pg_input_error_info('no_such_table', 'regclass');

-- a type name that denotes nothing is an error, not a row
SELECT pg_input_is_valid('x', 'nosuchtype');
SELECT * FROM pg_input_error_info('x', 'nosuchtype');

-- so is a modifier the type could never accept: it describes the type spec,
-- not the value
SELECT pg_input_is_valid('abc', 'varchar(0)');
SELECT pg_input_is_valid('1', 'numeric(0)');
SELECT pg_input_is_valid('1', 'numeric(5,1001)');
