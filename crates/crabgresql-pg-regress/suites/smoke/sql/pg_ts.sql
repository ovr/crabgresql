--
-- The five pg_ts_* catalogs: text-search parsers, dictionary templates,
-- dictionaries, configurations and their token maps.
--
-- Half of these rows are generated from the vendored .dat files -- the default
-- parser, four templates, and the simple dictionary and configuration. The
-- other half is the twenty-nine snowball languages, which initdb creates by
-- running snowball_create.sql rather than from catalog data, and which this
-- build reconstructs from the language list.
--
-- What is described here is not what this server can do: there is no stemmer,
-- so `english` exists as a configuration and to_tsvector does not lex through
-- it. Publishing the rows is still the better answer than five empty relations
-- -- \dF reads them, and the token-type numbering is visible nowhere else.
--
-- No raw OID is compared: the snowball band comes from initdb's counter rather
-- than from vendored data, so it moves between major versions. Generated with
-- psql -q -a against PostgreSQL 18.4.
--
-- The one parser, and the functions it is built from.
SELECT prsname, prsstart, prstoken, prsend, prsheadline, prslextype
  FROM pg_ts_parser ORDER BY prsname;
-- The five templates. Four come from the .dat; snowball is initdb's, and the
-- twenty-nine reconstructed dictionaries point at it.
SELECT tmplname, tmplinit, tmpllexize FROM pg_ts_template ORDER BY tmplname;
-- Thirty dictionaries and thirty configurations, exactly as a stock server has.
SELECT count(*) AS dicts FROM pg_ts_dict;
SELECT count(*) AS configs FROM pg_ts_config;
SELECT count(*) AS config_map_rows FROM pg_ts_config_map;
-- A language's dictionary names its template and the options CREATE TEXT
-- SEARCH DICTIONARY stored; a language without a stop-word list stores only
-- the language.
SELECT d.dictname, t.tmplname, d.dictinitoption
  FROM pg_ts_dict d JOIN pg_ts_template t ON t.oid = d.dicttemplate
 WHERE d.dictname IN ('simple', 'english_stem', 'russian_stem', 'greek_stem')
 ORDER BY d.dictname;
-- Every configuration uses the default parser: snowball supplies dictionaries,
-- not a parser.
SELECT p.prsname, count(*) AS configs
  FROM pg_ts_config c JOIN pg_ts_parser p ON p.oid = c.cfgparser
 GROUP BY p.prsname;
-- The token map of one configuration, by name -- the join \dF+ makes. Six
-- word-shaped token types go to the language's stemmer and the other thirteen
-- to simple.
SELECT c.cfgname, m.maptokentype, m.mapseqno, d.dictname
  FROM pg_ts_config_map m JOIN pg_ts_config c ON c.oid = m.mapcfg
  JOIN pg_ts_dict d ON d.oid = m.mapdict
 WHERE c.cfgname = 'english' ORDER BY m.maptokentype;
-- ...and the same split, counted over every configuration: 30 x 19 rows, of
-- which 30 x 6 reach a stemmer.
SELECT (SELECT count(*) FROM pg_ts_config_map m
          JOIN pg_ts_dict d ON d.oid = m.mapdict
         WHERE d.dictname <> 'simple') AS stemmed,
       (SELECT count(*) FROM pg_ts_config_map m
          JOIN pg_ts_dict d ON d.oid = m.mapdict
         WHERE d.dictname = 'simple') AS unstemmed;
-- Every configuration maps the same nineteen token types, once each.
SELECT count(*) AS wrong_token_count FROM (
  SELECT mapcfg FROM pg_ts_config_map GROUP BY mapcfg HAVING count(*) <> 19) AS bad;
SELECT count(*) AS duplicate_seqno FROM (
  SELECT mapcfg, maptokentype FROM pg_ts_config_map
   GROUP BY mapcfg, maptokentype HAVING count(*) > 1) AS dups;
-- Nothing points into thin air.
SELECT count(*) AS dangling_template FROM pg_ts_dict d
 WHERE NOT EXISTS (SELECT 1 FROM pg_ts_template t WHERE t.oid = d.dicttemplate);
SELECT count(*) AS dangling_parser FROM pg_ts_config c
 WHERE NOT EXISTS (SELECT 1 FROM pg_ts_parser p WHERE p.oid = c.cfgparser);
SELECT count(*) AS dangling_cfg FROM pg_ts_config_map m
 WHERE NOT EXISTS (SELECT 1 FROM pg_ts_config c WHERE c.oid = m.mapcfg);
SELECT count(*) AS dangling_dict FROM pg_ts_config_map m
 WHERE NOT EXISTS (SELECT 1 FROM pg_ts_dict d WHERE d.oid = m.mapdict);
SELECT count(*) AS dangling_lexize FROM pg_ts_template t
 WHERE NOT EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = t.tmpllexize);
-- The comments initdb writes with COMMENT ON, which no .dat carries.
SELECT obj_description(oid, 'pg_ts_dict') AS dict_description
  FROM pg_ts_dict WHERE dictname = 'english_stem';
SELECT obj_description(oid, 'pg_ts_config') AS config_description
  FROM pg_ts_config WHERE cfgname = 'english';
SELECT obj_description(oid, 'pg_ts_template') AS template_description
  FROM pg_ts_template WHERE tmplname = 'snowball';
-- Everything lives in pg_catalog, owned by the bootstrap role.
SELECT count(*) AS misplaced FROM pg_ts_dict
 WHERE dictnamespace <> 'pg_catalog'::regnamespace OR dictowner <> 10;
SELECT count(*) AS misplaced FROM pg_ts_config
 WHERE cfgnamespace <> 'pg_catalog'::regnamespace OR cfgowner <> 10;
