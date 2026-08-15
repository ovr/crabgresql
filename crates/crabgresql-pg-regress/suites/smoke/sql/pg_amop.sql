--
-- pg_amop / pg_amproc: what an operator family gives its access method.
--
-- pg_opclass says a type is indexable; these two say how. Nothing here pins a
-- raw OID from either relation: upstream's codegen numbers every one of their
-- rows, so inserting an entry anywhere in the data moves all the numbers below
-- it, and the row counts themselves differ between majors (18.4 ships 945
-- pg_amop rows, 19devel 951). What a client reads is names and shapes, so that
-- is what this file compares. Generated with psql -q -a against PostgreSQL 18.4.
--
-- The btree support functions of a family, by name -- the join \dAp makes.
-- The name comes from pg_proc rather than from rendering the regproc column,
-- because regprocout schema-qualifies a name more than one function carries
-- (support function 3 here is one of the many in_range overloads) and this
-- build resolves the name at codegen time instead of at output time.
SELECT f.opfname, p.amproclefttype::regtype, p.amprocrighttype::regtype,
       p.amprocnum, pr.proname
  FROM pg_amproc p JOIN pg_opfamily f ON f.oid = p.amprocfamily
  JOIN pg_am a ON a.oid = f.opfmethod
  JOIN pg_proc pr ON pr.oid = p.amproc
 WHERE f.opfname = 'integer_ops' AND a.amname = 'btree'
   AND p.amproclefttype = 'int4'::regtype
 ORDER BY p.amprocnum, p.amprocrighttype::regtype::text;
-- The same family under hash, which needs one function and a different one.
-- Both names are unique, so here the regproc column renders them itself.
SELECT f.opfname, p.amprocnum, p.amproc
  FROM pg_amproc p JOIN pg_opfamily f ON f.oid = p.amprocfamily
  JOIN pg_am a ON a.oid = f.opfmethod
 WHERE f.opfname = 'integer_ops' AND a.amname = 'hash'
   AND p.amproclefttype = 'int4'::regtype
 ORDER BY p.amprocnum;
-- Every btree strategy of a family, and that each is answered by its own
-- operator. The operator's name comes from pg_operator; until that is served
-- the row count is what says the five strategies are all distinct.
SELECT p.amopstrategy, p.amoppurpose, count(DISTINCT p.amopopr) AS operators
  FROM pg_amop p JOIN pg_opfamily f ON f.oid = p.amopfamily
  JOIN pg_am a ON a.oid = f.opfmethod
 WHERE f.opfname = 'integer_ops' AND a.amname = 'btree'
   AND p.amoplefttype = 'int4'::regtype AND p.amoprighttype = 'int4'::regtype
 GROUP BY p.amopstrategy, p.amoppurpose ORDER BY p.amopstrategy;
-- An ordering operator is the only kind that names a sort family, and it names
-- one that exists. These are gist's distance operators.
SELECT a.amname, f.opfname, sf.opfname AS sortfamily, count(*) AS entries
  FROM pg_amop p JOIN pg_opfamily f ON f.oid = p.amopfamily
  JOIN pg_am a ON a.oid = f.opfmethod
  JOIN pg_opfamily sf ON sf.oid = p.amopsortfamily
 WHERE p.amoppurpose = 'o'
 GROUP BY a.amname, f.opfname, sf.opfname ORDER BY a.amname, f.opfname;
-- A search operator names no sort family at all.
SELECT count(*) AS search_with_sortfamily FROM pg_amop
 WHERE amoppurpose = 's' AND amopsortfamily <> 0;
-- Nothing points into thin air.
SELECT count(*) AS dangling_family FROM pg_amop p
 WHERE NOT EXISTS (SELECT 1 FROM pg_opfamily f WHERE f.oid = p.amopfamily);
SELECT count(*) AS dangling_method FROM pg_amop p
 WHERE NOT EXISTS (SELECT 1 FROM pg_am a WHERE a.oid = p.amopmethod);
SELECT count(*) AS dangling_lefttype FROM pg_amop p
 WHERE NOT EXISTS (SELECT 1 FROM pg_type t WHERE t.oid = p.amoplefttype);
SELECT count(*) AS dangling_family FROM pg_amproc p
 WHERE NOT EXISTS (SELECT 1 FROM pg_opfamily f WHERE f.oid = p.amprocfamily);
SELECT count(*) AS dangling_proc FROM pg_amproc p
 WHERE NOT EXISTS (SELECT 1 FROM pg_proc pr WHERE pr.oid = p.amproc);
-- The claim the pair exists to make: every default btree operator class has a
-- comparison support function, so btree could actually use it.
SELECT count(*) AS classes_without_cmp FROM pg_opclass oc
 WHERE oc.opcdefault AND oc.opcmethod = (SELECT oid FROM pg_am WHERE amname = 'btree')
   AND NOT EXISTS (SELECT 1 FROM pg_amproc p
                    WHERE p.amprocfamily = oc.opcfamily
                      AND p.amproclefttype = oc.opcintype AND p.amprocnum = 1);
-- An operator family's entries all belong to the family's own access method.
SELECT count(*) AS method_mismatch FROM pg_amop p
  JOIN pg_opfamily f ON f.oid = p.amopfamily
 WHERE f.opfmethod <> p.amopmethod;
