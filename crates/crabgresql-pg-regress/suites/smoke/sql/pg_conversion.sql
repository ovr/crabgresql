--
-- pg_conversion: the built-in encoding conversions.
--
-- This server speaks UTF-8 and nothing else, so not one of these conversions
-- can ever run -- the widest gap of the catalog wave. The rows are still worth
-- publishing: an empty pg_conversion would claim PostgreSQL defines none, and
-- that is a different and false statement.
--
-- The total is not compared. 19devel's catalog data defines 98 conversions
-- where PostgreSQL 18.4 ships 128, so a count here would pin which major
-- version the data came from rather than anything about this build. Generated
-- with psql -q -a against PostgreSQL 18.4.
--
-- The encodings are stored as numbers; pg_encoding_to_char is how a client
-- reads them back, and it is what says the numbering here is PostgreSQL's.
SELECT conname, pg_encoding_to_char(conforencoding) AS source,
       pg_encoding_to_char(contoencoding) AS target, conproc, condefault
  FROM pg_conversion
 WHERE conname IN ('koi8_r_to_windows_1251', 'koi8_r_to_utf8', 'utf8_to_koi8_r',
                   'euc_jp_to_utf8', 'iso_8859_5_to_utf8')
 ORDER BY conname;
-- Every conversion named *_to_utf8 really does target UTF8, and vice versa --
-- the one encoding number this build can check against something other than
-- the table it transcribed, because it is the encoding it speaks.
SELECT count(*) AS mislabelled_target FROM pg_conversion
 WHERE conname LIKE '%\_to\_utf8' AND pg_encoding_to_char(contoencoding) <> 'UTF8';
SELECT count(*) AS mislabelled_source FROM pg_conversion
 WHERE conname LIKE 'utf8\_to\_%' AND pg_encoding_to_char(conforencoding) <> 'UTF8';
-- A conversion is between two different encodings, and every one of them is
-- the default for its pair.
SELECT count(*) AS self_conversion FROM pg_conversion
 WHERE conforencoding = contoencoding;
SELECT count(*) AS non_default FROM pg_conversion WHERE NOT condefault;
-- Nothing points into thin air, and every conversion function is a C function
-- taking six arguments -- these are the only rows in pg_proc with a probin at
-- all. The library *name* is not compared: 19devel renamed cyrillic_and_mic to
-- cyrillic, so the string differs between the vendored data and 18.4 while the
-- shape does not.
SELECT count(*) AS dangling_proc FROM pg_conversion c
 WHERE NOT EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = c.conproc);
SELECT DISTINCT p.prolang, p.provolatile, p.pronargs, p.probin IS NOT NULL AS has_library
  FROM pg_conversion c JOIN pg_proc p ON p.oid = c.conproc;
-- Everything lives in pg_catalog, owned by the bootstrap role.
SELECT count(*) AS misplaced FROM pg_conversion
 WHERE connamespace <> 'pg_catalog'::regnamespace OR conowner <> 10;
-- The descriptions PostgreSQL ships with them.
SELECT obj_description(oid, 'pg_conversion') AS description
  FROM pg_conversion WHERE conname = 'koi8_r_to_windows_1251';
