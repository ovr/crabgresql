--
-- pg_aggregate: what makes an aggregate function an aggregate.
--
-- As with pg_operator, nothing in this build reads the table -- the executor's
-- accumulators are its own -- so a row describes upstream's aggregate rather
-- than the plan this server runs. The queries below check the overlap from
-- both ends: the row describing an aggregate, and the same aggregate computing
-- an answer. Generated with psql -q -a against PostgreSQL 18.4.
--
-- The relation has no oid of its own: an aggregate is keyed by the pg_proc row
-- it extends, which is why aggfnoid is joined rather than rendered (regprocout
-- schema-qualifies a name more than one function carries, and every aggregate
-- name here is overloaded). The argument list is shown as the raw oidvector
-- pg_proc stores: subscripting an alias-qualified column is not supported here
-- yet, and these type OIDs are hand-assigned upstream and do not move.
SELECT p.proname, p.proargtypes, a.aggkind, a.aggnumdirectargs,
       t.proname AS transfn, a.aggtranstype::regtype,
       f.proname AS finalfn, a.aggfinalmodify
  FROM pg_aggregate a JOIN pg_proc p ON p.oid = a.aggfnoid
  JOIN pg_proc t ON t.oid = a.aggtransfn
  LEFT JOIN pg_proc f ON f.oid = a.aggfinalfn
 WHERE p.proname IN ('avg', 'max', 'string_agg')
   AND p.proargtypes::text IN ('20', '23', '25', '25 25')
 ORDER BY p.proname, p.proargtypes::text;
-- ...and the same aggregates, computed.
CREATE TABLE agg_t (i int, t text);
INSERT INTO agg_t VALUES (1, 'a'), (2, 'b'), (3, 'c');
SELECT count(*), min(i), max(i), sum(i), avg(i), string_agg(t, ',') FROM agg_t;
-- MIN and MAX name the ordering operator they are equivalent to. Everything
-- else that names one is an ordered-set aggregate, which is a different use of
-- the column; the plain aggregates that name none store 0.
SELECT p.proname, p.proargtypes, o.oprname, o.oprleft::regtype
  FROM pg_aggregate a JOIN pg_proc p ON p.oid = a.aggfnoid
  JOIN pg_operator o ON o.oid = a.aggsortop
 WHERE p.proname IN ('min', 'max') AND o.oprleft = 'int4'::regtype
 ORDER BY p.proname;
SELECT count(*) AS sorted_plain_non_extremum FROM pg_aggregate a
  JOIN pg_proc p ON p.oid = a.aggfnoid
 WHERE a.aggsortop <> 0 AND a.aggkind = 'n' AND p.proname NOT IN ('min', 'max');
-- A moving-window aggregate names a transition function and its inverse
-- together: one without the other is a frame that could add rows and never
-- remove them.
SELECT count(*) AS half_a_moving_aggregate FROM pg_aggregate
 WHERE (aggmtransfn = 0) <> (aggminvtransfn = 0);
-- The initial state is text, and NULL when the aggregate starts from nothing.
SELECT p.proname, p.proargtypes, a.agginitval, a.aggminitval, a.aggtransspace
  FROM pg_aggregate a JOIN pg_proc p ON p.oid = a.aggfnoid
 WHERE p.proname = 'avg' AND a.agginitval IS NOT NULL
 ORDER BY p.proargtypes::text;
-- Nothing points into thin air, and every key really is an aggregate.
SELECT count(*) AS dangling_key FROM pg_aggregate a
 WHERE NOT EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = a.aggfnoid);
SELECT count(*) AS not_an_aggregate FROM pg_aggregate a
  JOIN pg_proc p ON p.oid = a.aggfnoid WHERE p.prokind <> 'a';
SELECT count(*) AS missing_transfn FROM pg_aggregate WHERE aggtransfn = 0;
SELECT count(*) AS dangling_transfn FROM pg_aggregate a
 WHERE NOT EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = a.aggtransfn);
SELECT count(*) AS dangling_transtype FROM pg_aggregate a
 WHERE NOT EXISTS (SELECT 1 FROM pg_type t WHERE t.oid = a.aggtranstype);
SELECT count(*) AS dangling_sortop FROM pg_aggregate a
 WHERE a.aggsortop <> 0
   AND NOT EXISTS (SELECT 1 FROM pg_operator o WHERE o.oid = a.aggsortop);
-- Only an ordered-set or hypothetical-set aggregate takes direct arguments.
SELECT count(*) AS plain_with_direct_args FROM pg_aggregate
 WHERE aggkind = 'n' AND aggnumdirectargs <> 0;
DROP TABLE agg_t;
