--
-- pg_catalog coverage, wave A: the relations whose subsystem does not exist
-- here — command progress, replication, privileges and security labels, large
-- objects, event triggers, transforms, range types, two-phase commit.
--
-- Every one of them is empty, and on a PostgreSQL nobody used those features in
-- the answer is the same, so this file's expected output was generated with
-- psql -q -a against PostgreSQL 18.4 and holds for both servers.
--
-- Two things are checked per relation. `select *` pins the column names and
-- their order: that is what a client reads out of the row description, and it
-- is visible even with no rows. The `where` clauses below pin the column
-- *types*: a predicate compares a column against a literal of the declared
-- type, so a column typed text where PostgreSQL says pg_lsn would fail to bind
-- instead of quietly answering zero. (pg_attribute cannot be asked here — this
-- build reflects only user relations into it, not the catalogs themselves.)
--
-- Progress views. Empty in PostgreSQL too unless the command is running right
-- now, which is why a stub is a correct answer and not a placeholder.
SELECT * FROM pg_stat_progress_analyze;
SELECT * FROM pg_stat_progress_basebackup;
SELECT * FROM pg_stat_progress_cluster;
SELECT * FROM pg_stat_progress_copy;
SELECT * FROM pg_stat_progress_create_index;
SELECT * FROM pg_stat_progress_vacuum;
SELECT (SELECT count(*) FROM pg_stat_progress_analyze
          WHERE pid = 0 AND datid = 0 AND datname = 'x'
            AND sample_blks_total > 0 AND delay_time > 0.0) AS analyze_typed,
       (SELECT count(*) FROM pg_stat_progress_basebackup
          WHERE pid = 0 AND phase = 'x' AND backup_streamed > 0) AS basebackup_typed,
       (SELECT count(*) FROM pg_stat_progress_cluster
          WHERE relid = 0 AND command = 'x' AND heap_tuples_scanned > 0) AS cluster_typed,
       (SELECT count(*) FROM pg_stat_progress_copy
          WHERE relid = 0 AND type = 'x' AND tuples_processed > 0) AS copy_typed,
       (SELECT count(*) FROM pg_stat_progress_create_index
          WHERE index_relid = 0 AND current_locker_pid > 0
            AND partitions_done > 0) AS create_index_typed,
       (SELECT count(*) FROM pg_stat_progress_vacuum
          WHERE relid = 0 AND phase = 'x' AND dead_tuple_bytes > 0
            AND delay_time > 0.0) AS vacuum_typed;
-- Replication. One server, no slots, nothing streaming: the WAL this build
-- writes is for crash recovery, so there is no LSN here to report.
SELECT * FROM pg_replication_origin;
SELECT * FROM pg_replication_origin_status;
SELECT * FROM pg_replication_slots;
SELECT * FROM pg_stat_replication;
SELECT * FROM pg_stat_replication_slots;
SELECT * FROM pg_stat_subscription;
SELECT * FROM pg_stat_subscription_stats;
SELECT * FROM pg_stat_wal_receiver;
SELECT * FROM pg_subscription;
SELECT * FROM pg_subscription_rel;
SELECT (SELECT count(*) FROM pg_replication_origin
          WHERE roident = 0 AND roname = 'x') AS origin_typed,
       (SELECT count(*) FROM pg_replication_origin_status
          WHERE local_id = 0 AND remote_lsn > '0/0'::pg_lsn) AS origin_status_typed,
       (SELECT count(*) FROM pg_replication_slots
          WHERE slot_name = 'x' AND temporary AND active_pid = 0
            AND xmin = '1'::xid AND restart_lsn > '0/0'::pg_lsn
            AND safe_wal_size > 0
            AND inactive_since > '2000-01-01'::timestamptz) AS slots_typed,
       (SELECT count(*) FROM pg_stat_replication
          WHERE usesysid = 0 AND client_addr = '127.0.0.1'::inet
            AND backend_xmin = '1'::xid AND sent_lsn > '0/0'::pg_lsn
            AND write_lag > '1 second'::interval
            AND reply_time > '2000-01-01'::timestamptz) AS stat_replication_typed,
       (SELECT count(*) FROM pg_stat_replication_slots
          WHERE slot_name = 'x' AND total_bytes > 0) AS slot_stats_typed;
SELECT (SELECT count(*) FROM pg_stat_subscription
          WHERE subid = 0 AND subname = 'x' AND leader_pid = 0
            AND received_lsn > '0/0'::pg_lsn) AS subscription_typed,
       (SELECT count(*) FROM pg_stat_subscription_stats
          WHERE subid = 0 AND confl_multiple_unique_conflicts > 0) AS sub_stats_typed,
       (SELECT count(*) FROM pg_stat_wal_receiver
          WHERE receive_start_tli = 0 AND written_lsn > '0/0'::pg_lsn
            AND conninfo = 'x') AS wal_receiver_typed,
       (SELECT count(*) FROM pg_subscription
          WHERE subdbid = 0 AND subskiplsn > '0/0'::pg_lsn AND subenabled
            AND substream = 'f' AND subpublications = '{a}'::text[]) AS subscription_cat_typed,
       (SELECT count(*) FROM pg_subscription_rel
          WHERE srrelid = 0 AND srsubstate = 'r'
            AND srsublsn > '0/0'::pg_lsn) AS subscription_rel_typed;
-- Privileges and labels. No GRANT, no SECURITY LABEL, no ALTER ROLE ... SET.
SELECT * FROM pg_default_acl;
SELECT * FROM pg_parameter_acl;
SELECT * FROM pg_shdepend;
SELECT * FROM pg_seclabel;
SELECT * FROM pg_shseclabel;
SELECT * FROM pg_seclabels;
SELECT * FROM pg_db_role_setting;
SELECT (SELECT count(*) FROM pg_default_acl
          WHERE defaclnamespace = 0 AND defaclobjtype = 'r') AS default_acl_typed,
       (SELECT count(*) FROM pg_parameter_acl WHERE parname = 'x') AS parameter_acl_typed,
       (SELECT count(*) FROM pg_shdepend
          WHERE dbid = 0 AND objsubid = 0 AND deptype = 'o') AS shdepend_typed,
       (SELECT count(*) FROM pg_seclabel
          WHERE objsubid = 0 AND provider = 'x' AND label = 'y') AS seclabel_typed,
       (SELECT count(*) FROM pg_shseclabel
          WHERE classoid = 0 AND provider = 'x') AS shseclabel_typed,
       (SELECT count(*) FROM pg_seclabels
          WHERE objnamespace = 0 AND objtype = 'x') AS seclabels_typed,
       (SELECT count(*) FROM pg_db_role_setting
          WHERE setrole = 0 AND setconfig = '{a=b}'::text[]) AS db_role_setting_typed;
-- Three of wave A are NOT empty on a stock PostgreSQL, so `select *` would
-- diverge and only the column names are pinned, under a predicate that is empty
-- on both servers. pg_init_privs holds ~228 rows there — the initial ACL of
-- every system catalog; this build serves its catalogs from a registry rather
-- than reflecting them into pg_class, so no such object exists to record.
SELECT objoid, classoid, objsubid, privtype, initprivs
  FROM pg_init_privs WHERE privtype = 'x';
-- pg_shdescription holds three rows there: initdb's comments on template1,
-- template0 and postgres. A crabgresql server serves exactly one database and
-- has no COMMENT ON DATABASE to have written one.
SELECT objoid, classoid, description FROM pg_shdescription WHERE classoid = 0;
-- pg_range holds six rows there, one per built-in range type. This build models
-- no range type, so a row would name an rngtypid pg_type cannot resolve and the
-- usual join to pg_type would return a broken pair instead of nothing.
SELECT rngtypid, rngsubtype, rngmultitypid, rngcollation, rngsubopc,
       rngcanonical, rngsubdiff
  FROM pg_range WHERE rngtypid = 0;
-- The rest of wave A: large objects, event triggers, transforms, 2PC.
SELECT * FROM pg_largeobject;
SELECT * FROM pg_largeobject_metadata;
SELECT * FROM pg_event_trigger;
SELECT * FROM pg_transform;
SELECT * FROM pg_prepared_xacts;
SELECT (SELECT count(*) FROM pg_largeobject
          WHERE loid = 0 AND pageno = 0 AND data = '\x00'::bytea) AS largeobject_typed,
       (SELECT count(*) FROM pg_largeobject_metadata
          WHERE lomowner = 0) AS lo_metadata_typed,
       (SELECT count(*) FROM pg_event_trigger
          WHERE evtname = 'x' AND evtenabled = 'O'
            AND evttags = '{a}'::text[]) AS event_trigger_typed,
       (SELECT count(*) FROM pg_transform
          WHERE trftype = 0 AND trflang = 0) AS transform_typed,
       (SELECT count(*) FROM pg_prepared_xacts
          WHERE transaction = '1'::xid AND gid = 'x'
            AND prepared > '2000-01-01'::timestamptz AND owner = 'y') AS prepared_xacts_typed;
-- Every relation of the wave answers to its name as a regclass, which is what a
-- client casts before joining one.
SELECT count(*) AS wave_a_relations FROM (VALUES
  ('pg_stat_progress_analyze'), ('pg_stat_progress_basebackup'),
  ('pg_stat_progress_cluster'), ('pg_stat_progress_copy'),
  ('pg_stat_progress_create_index'), ('pg_stat_progress_vacuum'),
  ('pg_replication_origin'), ('pg_replication_origin_status'),
  ('pg_replication_slots'), ('pg_stat_replication'),
  ('pg_stat_replication_slots'), ('pg_stat_subscription'),
  ('pg_stat_subscription_stats'), ('pg_stat_wal_receiver'),
  ('pg_subscription'), ('pg_subscription_rel'),
  ('pg_default_acl'), ('pg_init_privs'), ('pg_parameter_acl'), ('pg_shdepend'),
  ('pg_seclabel'), ('pg_shseclabel'), ('pg_seclabels'), ('pg_shdescription'),
  ('pg_db_role_setting'), ('pg_largeobject'), ('pg_largeobject_metadata'),
  ('pg_event_trigger'), ('pg_transform'), ('pg_range'), ('pg_prepared_xacts')
) AS t(name) WHERE t.name::regclass::oid > 0;
