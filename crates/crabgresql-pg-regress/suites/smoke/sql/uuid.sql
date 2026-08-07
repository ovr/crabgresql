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
