--
-- UUID
-- uuid: input parsing (canonical / unpunctuated / braced / uppercase), the
-- canonical lowercase output, casts to and from text, and comparison in WHERE
-- and ORDER BY. Output hand-checked against PostgreSQL's aligned format.
--
-- typed-literal output; the default column name of a typed literal is the type
SELECT uuid 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11';
-- the accepted input spellings all normalize to the canonical lowercase form
SELECT uuid 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11' AS upper,
       uuid 'a0eebc999c0b4ef8bb6d6bb9bd380a11' AS unpunctuated,
       uuid '{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}' AS braced;

-- casts: text -> uuid and uuid -> text (default column name follows the cast)
SELECT '11111111-1111-1111-1111-111111111111'::uuid;
SELECT (uuid '11111111-1111-1111-1111-111111111111')::text;

-- comparison drives WHERE and ORDER BY (raw byte order)
SELECT uuid '00000000-0000-0000-0000-000000000001'
     = uuid '00000000-0000-0000-0000-000000000001' AS eq,
       uuid '00000000-0000-0000-0000-000000000001'
     < uuid '00000000-0000-0000-0000-000000000002' AS lt;

SELECT u FROM (VALUES
    (uuid '22222222-2222-2222-2222-222222222222'),
    (uuid '00000000-0000-0000-0000-000000000000'),
    (uuid '11111111-1111-1111-1111-111111111111')
) AS t(u) ORDER BY 1;

-- invalid input is 22P02, echoing the offending text
SELECT uuid 'not-a-uuid';
SELECT 'a0eebc99'::uuid;

--
-- generation and extraction.
--
-- Every assertion below is a predicate rather than a printed uuid: the values
-- are random, so only their invariants are stable.
--
-- version 4 (gen_random_uuid and its RFC 9562 spelling uuidv4) and version 7
SELECT uuid_extract_version(gen_random_uuid()) AS v4,
       uuid_extract_version(uuidv4())          AS v4_alias,
       uuid_extract_version(uuidv7())          AS v7;
-- 100 draws with no repeat (a volatile generator is evaluated per row, not
-- hoisted out of the scan), and the RFC 9562 variant nibble in every one
SELECT count(DISTINCT g) FROM (SELECT gen_random_uuid() AS g FROM generate_series(1, 100)) s;
SELECT count(*) AS wrong_variant
  FROM (SELECT uuidv4() AS g FROM generate_series(1, 100)) s
 WHERE substring(g::text, 20, 1) NOT IN ('8', '9', 'a', 'b');

-- a version 7 value stamps the current instant
SELECT uuid_extract_timestamp(uuidv7())
         BETWEEN now() - interval '1 minute' AND clock_timestamp() + interval '1 second'
       AS v7_is_now;

-- and successive values sort in generation order — both forms, since each
-- draws from the same monotonic guard
CREATE TABLE uuid_v7 (id int, g uuid, shifted uuid);
INSERT INTO uuid_v7 SELECT i, uuidv7(), uuidv7(interval '1 day') FROM generate_series(1, 50) i;
SELECT count(*) AS out_of_order FROM uuid_v7 a JOIN uuid_v7 b ON a.id < b.id WHERE a.g >= b.g;
SELECT count(*) AS shifted_out_of_order
  FROM uuid_v7 a JOIN uuid_v7 b ON a.id < b.id WHERE a.shifted >= b.shifted;
SELECT count(DISTINCT g), count(DISTINCT shifted) FROM uuid_v7;
DROP TABLE uuid_v7;

-- the shift argument moves the calendar, so months and years are not a fixed
-- count of microseconds
SELECT uuid_extract_timestamp(uuidv7(interval '1 day'))   > now() + interval '23 hours' AS day_shift,
       uuid_extract_timestamp(uuidv7(interval '1 month')) > now() + interval '27 days'  AS month_shift,
       uuid_extract_timestamp(uuidv7(interval '1000 years'))
         > now() + interval '999 years' AS far_forward,
       uuid_extract_version(uuidv7(interval '1 day')) AS still_v7;

-- extraction against the RFC 9562 test vectors, which both name the same instant
SELECT uuid_extract_timestamp('C232AB00-9414-11EC-B3C8-9F6BDECED846')
         = timestamptz '2022-02-22 14:22:22-05' AS v1_vector,
       uuid_extract_timestamp('017F22E2-79B0-7CC3-98C4-DC0C0C07398F')
         = timestamptz '2022-02-22 14:22:22-05' AS v7_vector;
-- a version without a timestamp field, a non-RFC-9562 variant, and NULL all
-- answer NULL rather than erroring
SELECT uuid_extract_timestamp('11111111-1111-4111-8111-111111111111') IS NULL AS v4_has_no_timestamp,
       uuid_extract_version('11111111-1111-1111-1111-111111111111')   IS NULL AS non_rfc_variant,
       uuid_extract_timestamp(NULL::uuid) IS NULL AS strict_null;
SELECT uuid_extract_version('11111111-1111-5111-8111-111111111111');

--
-- The four uuidv7 range errors and the uuid/bytea casts below are hand-written
-- from vendor/postgres/regress/expected/uuid.out (:272-284, :350-366) rather
-- than generated from the local PostgreSQL 18.4, which predates both: 18.4 has
-- no bytea cast at all, and it wraps the 48-bit timestamp field instead of
-- rejecting the shift.
--
-- uuidv7: an infinite interval has no instant to stamp
SELECT uuidv7('infinity'::interval);
SELECT uuidv7('-infinity'::interval);
-- uuidv7: before the Unix epoch, and past the 48-bit millisecond field
SELECT uuidv7('-1000 years'::interval);
SELECT uuidv7('9000 years'::interval);

-- casts: the 16 stored bytes, unchanged in either direction
SELECT '5b35380a-7143-4912-9b55-f322699c6770'::uuid::bytea;
SELECT '\x019a2f859ced7225b99d9c55044a2563'::bytea::uuid;
SELECT '\x1234567890abcdef'::bytea::uuid;
SELECT g = g::bytea::uuid AS roundtrip FROM (SELECT gen_random_uuid() AS g) s;
