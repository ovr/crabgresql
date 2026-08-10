--
-- HASH
-- The core digest functions: sha224/sha256/sha384/sha512 and crc32/crc32c.
-- All six take bytea only and are strict. Output generated from PostgreSQL.
--

-- SHA-2 against the FIPS-style vectors PG's own `strings` test pins
SELECT sha224('');
SELECT sha224('The quick brown fox jumps over the lazy dog.');
SELECT sha256('');
SELECT sha256('The quick brown fox jumps over the lazy dog.');
SELECT sha384('');
SELECT sha384('The quick brown fox jumps over the lazy dog.');
SELECT sha512('');
SELECT sha512('The quick brown fox jumps over the lazy dog.');

-- the argument is bytes, not characters: a multi-byte literal and the same
-- bytes written as an escape must agree
SELECT sha256('café'::bytea) = sha256('caf\303\251'::bytea) AS same;

-- CRC. The result is int8 because the checksum is unsigned 32-bit.
SELECT crc32('');
SELECT crc32('The quick brown fox jumps over the lazy dog.');
SELECT crc32c('');
SELECT crc32c('The quick brown fox jumps over the lazy dog.');

-- lengths straddling the 128-byte block the hardware CRC-32C path consumes;
-- the 129-byte case exceeds int4 and pins the widening
SELECT crc32c(repeat('A', 127)::bytea);
SELECT crc32c(repeat('A', 128)::bytea);
SELECT crc32c(repeat('A', 129)::bytea);
SELECT crc32c(repeat('A', 800)::bytea);

-- strictness
SELECT sha256(NULL::bytea) IS NULL AS sha_null, crc32(NULL::bytea) IS NULL AS crc_null;

-- an unknown literal resolves through byteain, so a bare literal and the bytes
-- it denotes agree
SELECT sha256('abc') = sha256(decode('616263', 'hex')) AS same;

-- there is no text overload, so a typed text argument finds no candidate.
-- Overload-resolution errors carry no source span here, so the expected output
-- omits PG's `LINE 1:` caret (and the undefined-function HINT), as
-- `to_char.out` does.
SELECT sha256('abc'::text);
SELECT crc32('abc'::text);

-- the digest is bytea, so encode() re-renders it
SELECT encode(sha256('abc'), 'hex');
SELECT encode(sha224('abc'), 'base64');
-- digest widths
SELECT octet_length(sha512('abc')) AS len512, octet_length(sha384('abc')) AS len384,
       octet_length(sha256('abc')) AS len256, octet_length(sha224('abc')) AS len224;
