--
-- pg_operator: the built-in operators.
--
-- Nothing in this build reads this table -- operators are resolved by the
-- binder's own code -- so the rows are a description of what upstream's
-- operator set *is*, not an inventory of what this server evaluates. The two
-- overlap, and the queries below check the overlap in both directions: the row
-- describing an operator, and the same operator actually being run.
--
-- Generated with psql -q -a against PostgreSQL 18.4.
--
-- The rows behind the operators the expressions further down evaluate.
SELECT oprname, oprkind, oprleft::regtype, oprright::regtype, oprresult::regtype,
       oprcode, oprcanmerge, oprcanhash
  FROM pg_operator
 WHERE oprname IN ('=', '<', '+', '||') AND oprleft IN ('int4'::regtype, 'text'::regtype)
   AND oprright IN ('int4'::regtype, 'text'::regtype)
 ORDER BY oprname, oprleft::regtype::text, oprright::regtype::text;
-- ...and the same operators, evaluated.
SELECT 1 = 1 AS eq, 1 < 2 AS lt, 1 + 2 AS pl, 'a' || 'b' AS cat,
       'a' = 'a'::text AS texteq;
-- A prefix operator writes no left operand at all, which the catalog stores as
-- 0 and psql renders as `-`.
SELECT oprname, oprkind, oprleft, oprright::regtype, oprcode
  FROM pg_operator WHERE oprkind = 'l' AND oprright = 'int4'::regtype
 ORDER BY oprname;
-- A commutator commutes and a negator negates: both point back at the row that
-- named them, with the operands swapped for the commutator.
SELECT o.oprname, c.oprname AS commutator, n.oprname AS negator,
       c.oprleft::regtype AS com_left, c.oprright::regtype AS com_right
  FROM pg_operator o
  LEFT JOIN pg_operator c ON c.oid = o.oprcom
  LEFT JOIN pg_operator n ON n.oid = o.oprnegate
 WHERE o.oprname IN ('=', '<', '<=') AND o.oprleft = 'int4'::regtype
   AND o.oprright = 'int8'::regtype
 ORDER BY o.oprname;
SELECT count(*) AS asymmetric_commutators FROM pg_operator a
  JOIN pg_operator b ON b.oid = a.oprcom
 WHERE b.oprcom <> a.oid OR b.oprleft <> a.oprright OR b.oprright <> a.oprleft;
-- Nothing points into thin air. oprcode is required -- an operator nothing
-- evaluates is not an operator -- while the two selectivity estimators may be
-- absent, which the catalog stores as 0.
SELECT count(*) AS missing_code FROM pg_operator WHERE oprcode = 0;
SELECT count(*) AS dangling_code FROM pg_operator o
 WHERE NOT EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = o.oprcode);
SELECT count(*) AS dangling_result FROM pg_operator o
 WHERE NOT EXISTS (SELECT 1 FROM pg_type t WHERE t.oid = o.oprresult);
SELECT count(*) AS dangling_left FROM pg_operator o
 WHERE o.oprleft <> 0
   AND NOT EXISTS (SELECT 1 FROM pg_type t WHERE t.oid = o.oprleft);
-- oprkind and oprleft agree: exactly the prefix operators have no left operand.
SELECT count(*) AS kind_mismatch FROM pg_operator
 WHERE (oprkind = 'l') <> (oprleft = 0);
-- Every operator lives in pg_catalog, owned by the bootstrap role.
SELECT count(*) AS misplaced FROM pg_operator
 WHERE oprnamespace <> 'pg_catalog'::regnamespace OR oprowner <> 10;
-- Only an equality operator is mergeable or hashable, which is what tells the
-- planner it may build a merge or hash join on it. The totals are not compared:
-- they move between majors (19devel marks 56 mergeable against 18.4's 54).
SELECT count(*) AS non_equality_mergeable FROM pg_operator
 WHERE (oprcanmerge OR oprcanhash) AND oprname <> '=';
-- A description came with every operator: pg_description carries one per row.
SELECT obj_description(o.oid, 'pg_operator') AS description
  FROM pg_operator o WHERE o.oprname = '=' AND o.oprleft = 'int4'::regtype
   AND o.oprright = 'int4'::regtype;
