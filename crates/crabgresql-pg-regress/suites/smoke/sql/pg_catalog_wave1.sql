--
-- pg_catalog coverage, wave 1
-- The relations psql and pg_dump join against that this build did not serve:
-- the feature stubs (triggers, RLS, publications, extended statistics, foreign
-- data), the per-relkind views over pg_class (pg_tables/pg_views/pg_indexes/
-- pg_sequences), pg_rewrite, and the extension trio.
--
-- The stubs are empty because the feature does not exist here — and PostgreSQL
-- answers zero rows for the same query on a database nobody used that feature
-- in, so this file's expected output is the same on both servers. Generated
-- with psql -q -a against PostgreSQL 18.4.
--
CREATE TABLE cw_t (a int NOT NULL, b text, PRIMARY KEY (a));
CREATE INDEX cw_t_b_idx ON cw_t (b DESC);
CREATE SEQUENCE cw_fresh;
CREATE SEQUENCE cw_used START 5;
SELECT nextval('cw_used');
CREATE VIEW cw_v AS SELECT a, b FROM cw_t WHERE a > 1;
-- The per-relkind views. tablespace is NULL (the default tablespace), and
-- hasrules/hastriggers/rowsecurity agree with the pg_class flags.
SELECT schemaname, tablename, hasindexes, hasrules, hastriggers, rowsecurity
  FROM pg_tables WHERE tablename LIKE 'cw\_%' ORDER BY tablename;
SELECT schemaname, viewname, definition FROM pg_views WHERE viewname = 'cw_v';
-- indexdef is rendered by the same code pg_get_indexdef uses, so the view and
-- the function cannot disagree about an index.
SELECT tablename, indexname, tablespace, indexdef
  FROM pg_indexes WHERE tablename = 'cw_t' ORDER BY indexname;
SELECT indexname, indexdef = pg_get_indexdef(c.oid) AS view_matches_function
  FROM pg_indexes i JOIN pg_class c ON c.relname = i.indexname
 WHERE i.tablename = 'cw_t' ORDER BY indexname;
-- last_value is NULL until the sequence has been read from: a fresh sequence
-- and one that has handed out its start value hold the same counter.
SELECT schemaname, sequencename, data_type, start_value, increment_by, cycle,
       cache_size, last_value
  FROM pg_sequences WHERE sequencename LIKE 'cw\_%' ORDER BY sequencename;
-- Every view carries a _RETURN rule, which is where its body is stored.
-- ev_action itself is not compared: PostgreSQL stores a serialized node tree
-- there and this build stores the deparsed SQL (the model every pg_node_tree
-- column here uses), so the two never render alike. What has to agree is that
-- the rule exists, carries a body, and that the body is the view definition —
-- which pg_get_viewdef reads back on both servers.
SELECT rulename, ev_type, ev_enabled, is_instead, ev_qual,
       ev_action IS NOT NULL AS has_action
  FROM pg_rewrite WHERE ev_class = 'cw_v'::regclass;
SELECT pg_get_viewdef('cw_v') = (SELECT definition FROM pg_views WHERE viewname = 'cw_v')
         AS viewdef_matches_pg_views;
-- pg_get_indexdef's per-column form: a key by number, bare; past the end is the
-- empty string, and an OID no index answers to is NULL.
SELECT pg_get_indexdef(c.oid, 1, true) AS key1,
       pg_get_indexdef(c.oid, 9, true) = '' AS past_end,
       pg_get_indexdef(0) IS NULL AS unknown_oid
  FROM pg_class c WHERE c.relname = 'cw_t_b_idx';
-- pg_partition_ancestors: the relation itself then each partitioned parent. A
-- relation that is neither a partition nor partitioned yields NO rows.
CREATE TABLE cw_p (a int) PARTITION BY RANGE (a);
CREATE TABLE cw_p1 PARTITION OF cw_p FOR VALUES FROM (0) TO (10);
SELECT relid FROM pg_partition_ancestors('cw_p1');
SELECT count(*) AS plain_table_rows FROM pg_partition_ancestors('cw_t');
-- The one extension, which really is installed.
SELECT extname, extnamespace, extrelocatable, extversion FROM pg_extension;
-- Filtered to the one this build has: a PostgreSQL install offers whatever is
-- in SHAREDIR/extension, and there is no such directory here.
SELECT name, default_version, installed_version FROM pg_available_extensions WHERE name = 'plpgsql';
SELECT name, version, installed, relocatable, schema FROM pg_available_extension_versions WHERE name = 'plpgsql';
-- The set-returning functions psql calls instead of those two views. The
-- versions one has eight columns where the view has nine: `installed` is the
-- view's own, computed from pg_extension rather than reported by the function.
SELECT * FROM pg_available_extensions() WHERE name = 'plpgsql';
SELECT * FROM pg_available_extension_versions() WHERE name = 'plpgsql';
-- pg_tablespace_location: the empty string for the bootstrap tablespaces, which
-- live inside the data directory. It never reads pg_tablespace — an OID with no
-- pg_tblspc entry is the failing stat, not NULL — and OID 0 answers the empty
-- string as well.
SELECT spcname, pg_tablespace_location(oid) = '' AS no_location
  FROM pg_tablespace ORDER BY oid;
SELECT pg_tablespace_location(0) = '' AS zero, pg_tablespace_location(NULL) IS NULL AS strict;
SELECT pg_tablespace_location(16384);           -- error
-- psql's \dx reads its Description column from pg_description, not from
-- pg_available_extensions.comment. The extension's row is one of the ~640 this
-- build publishes (PostgreSQL carries ~5400, the rest of them describing
-- catalogs this build does not serve, which is why the relation is not counted
-- here), and it matches.
SELECT objsubid, description FROM pg_description
 WHERE classoid = 'pg_extension'::regclass
   AND objoid = (SELECT oid FROM pg_extension WHERE extname = 'plpgsql');
-- The stubs. Each is empty because the feature it describes does not exist.
SELECT count(*) AS pg_trigger FROM pg_trigger;
SELECT count(*) AS pg_policy FROM pg_policy;
SELECT count(*) AS pg_policies FROM pg_policies;
SELECT count(*) AS pg_publication FROM pg_publication;
SELECT count(*) AS pg_publication_rel FROM pg_publication_rel;
SELECT count(*) AS pg_publication_namespace FROM pg_publication_namespace;
SELECT count(*) AS pg_publication_tables FROM pg_publication_tables;
SELECT count(*) AS pg_statistic_ext FROM pg_statistic_ext;
SELECT count(*) AS pg_statistic_ext_data FROM pg_statistic_ext_data;
SELECT count(*) AS pg_stats_ext FROM pg_stats_ext;
SELECT count(*) AS pg_stats_ext_exprs FROM pg_stats_ext_exprs;
SELECT count(*) AS pg_foreign_table FROM pg_foreign_table;
SELECT count(*) AS pg_foreign_server FROM pg_foreign_server;
SELECT count(*) AS pg_foreign_data_wrapper FROM pg_foreign_data_wrapper;
SELECT count(*) AS pg_user_mapping FROM pg_user_mapping;
SELECT count(*) AS pg_user_mappings FROM pg_user_mappings;
SELECT count(*) AS pg_matviews FROM pg_matviews;
-- pg_rules is deliberately absent: PostgreSQL ships two rows for its own system
-- views on a fresh cluster, and this build serves none, so the two disagree by
-- construction rather than by a bug worth pinning here.
DROP VIEW cw_v;
DROP TABLE cw_p1;
DROP TABLE cw_p;
DROP TABLE cw_t;
DROP SEQUENCE cw_fresh;
DROP SEQUENCE cw_used;
