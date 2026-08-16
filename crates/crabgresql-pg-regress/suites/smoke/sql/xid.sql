--
-- XID / XID8
-- The two transaction id types: input parsing (C `strtoul(base 0)` spellings
-- and each type's range rules), unsigned decimal output, the xid8 -> xid cast,
-- `xid8cmp`, and the capability split between them.
--
-- `xid` deliberately has a hash operator class but no btree one, because
-- transaction ids compare with modular arithmetic. So `=` and `<>` bind, and
-- GROUP BY / DISTINCT / UNION work, while `<`, ORDER BY, `min`/`max` and a
-- btree index on an `xid` column must all fail. `xid8` is an ordinary counter
-- and gets the full set. The negative half of that split is the part upstream
-- does not test, so it is covered here.
--
-- The first half is ported from vendor/postgres/regress/sql/xid.sql. What
-- remains unmodeled of that file is `pg_snapshot` and its visibility functions,
-- so upstream `xid` is not on the promotion list in
-- suites/upstream_must_pass.txt; `pg_current_xact_id*` and `pg_xact_status` are
-- covered by the last section here, in a shape that does not need `\gset`.
--
-- `operator does not exist` now carries PG's full report — the `LINE n: ... ^`
-- caret under the operator, the DETAIL, and the HINT. `function ... does not
-- exist` still has no caret: it is raised while binding an operand, which knows
-- no span of its own, so the caret is omitted rather than pointed at the wrong
-- token. That remaining gap is repo-wide, not specific to these types.
--

-- values in range, in octal, decimal, hex
select '010'::xid,
       '42'::xid,
       '0xffffffff'::xid,
       '-1'::xid,
	   '010'::xid8,
	   '42'::xid8,
	   '0xffffffffffffffff'::xid8,
	   '-1'::xid8;

-- garbage values
select ''::xid;
select 'asdf'::xid;
select ''::xid8;
select 'asdf'::xid8;

-- a run that stops at an invalid digit for its base is a syntax error, not a
-- partial parse: '08' is not octal, and neither '0b' nor '0o' is a prefix
select '08'::xid;
select '0b11'::xid;
select '0o17'::xid;

-- Also try it with non-error-throwing API
SELECT pg_input_is_valid('42', 'xid');
SELECT pg_input_is_valid('asdf', 'xid');
SELECT * FROM pg_input_error_info('0xffffffffff', 'xid');
SELECT pg_input_is_valid('42', 'xid8');
SELECT pg_input_is_valid('asdf', 'xid8');
SELECT * FROM pg_input_error_info('0xffffffffffffffffffff', 'xid8');

-- xid takes anything that fits int4 or uint4, negatives wrapping; the two
-- spellings of one wrapped value agree, and the gap between the bands errors
select '-2'::xid, '18446744073709551614'::xid, '-2147483648'::xid;
select '4294967296'::xid;
select '-2147483649'::xid;

-- xid8 spans the whole uint8, so a negative simply wraps
select '-9223372036854775808'::xid8, '4294967296'::xid8;
select '18446744073709551616'::xid8;

-- equality
select '1'::xid = '1'::xid;
select '1'::xid != '1'::xid;
select '1'::xid8 = '1'::xid8;
select '1'::xid8 != '1'::xid8;

-- PG also ships =(xid, int4) / <>(xid, int4), which is how `xmin = 12345` is
-- written. It is deliberately narrow: xid on the left only, int4 only (int2
-- widens into it), and the int is compared as a raw bit pattern -- so
-- '4294967295'::xid = -1 is true.
select '1'::xid = 1, '4294967295'::xid = -1, '1'::xid <> 2, '1'::xid = 1::int2;
select 1 = '1'::xid;      -- error: not commutative
select '1'::xid = 1::int8;  -- error: int4 only
select '1'::xid < 2;      -- error: still no ordering operator
select '1'::xid8 = 1;     -- error: xid8 has no int variant
-- the operator's coercion must not leak out as a user-written cast
select 1::xid;
select 1::int4::xid;
select 1::int2::xid;

-- conversion
select '1'::xid = '1'::xid8::xid;
select '1'::xid != '1'::xid8::xid;
-- the cast truncates to the low 32 bits rather than range-checking
select '4294967297'::xid8::xid;

-- we don't want relational operators for xid, due to use of modular arithmetic
select '1'::xid < '2'::xid;
select '1'::xid <= '2'::xid;
select '1'::xid > '2'::xid;
select '1'::xid >= '2'::xid;

-- we want them for xid8 though
select '1'::xid8 < '2'::xid8, '2'::xid8 < '2'::xid8, '2'::xid8 < '1'::xid8;
select '1'::xid8 <= '2'::xid8, '2'::xid8 <= '2'::xid8, '2'::xid8 <= '1'::xid8;
select '1'::xid8 > '2'::xid8, '2'::xid8 > '2'::xid8, '2'::xid8 > '1'::xid8;
select '1'::xid8 >= '2'::xid8, '2'::xid8 >= '2'::xid8, '2'::xid8 >= '1'::xid8;

-- we also have a 3way compare for btrees
select xid8cmp('1', '2'), xid8cmp('2', '2'), xid8cmp('2', '1');

-- min() and max() for xid8
create table xid8_t1 (x xid8);
insert into xid8_t1 values ('0'), ('010'), ('42'), ('0xffffffffffffffff'), ('-1');
select min(x), max(x) from xid8_t1;

-- xid8 has btree and hash opclasses; the btree one is physical, not
-- metadata-only, so an equality probe must plan as an Index Scan
create index on xid8_t1 using btree(x);
explain select x from xid8_t1 where x = '42'::xid8;
select x from xid8_t1 where x = '42'::xid8;
create index on xid8_t1 using hash(x);
drop table xid8_t1;

-- the other half of the split: xid has equality, so the dedup operations work
create table xid_t1 (x xid);
insert into xid_t1 values ('1'), ('2'), ('1'), ('-1');
select x, count(*) from xid_t1 group by x order by x::text;
-- DISTINCT and UNION dedup by equality alone, so neither needs an ordering
select count(*) from (select distinct x from xid_t1) s;
select count(*) from (select x from xid_t1 union select '7'::xid) s;

-- ... but no ordering, so these must all fail
select x from xid_t1 order by x;
select min(x) from xid_t1;
select max(x) from xid_t1;
create index on xid_t1 using btree(x);
drop table xid_t1;

-- a btree-keyed column of type xid is rejected at DDL time for the same reason
create table xid_pk (x xid primary key);

--
-- The transaction-id functions, and pg_is_in_recovery beside them. Every id is
-- different on every run, so nothing below prints one: the answers are all
-- predicates over them, and the one arithmetic check is a difference.
--

select pg_is_in_recovery();
select pg_typeof(txid_current()), pg_typeof(pg_current_xact_id()),
       pg_typeof(pg_is_in_recovery()), pg_typeof(pg_xact_status('2'::xid8));

-- under autocommit the statement that asks gets an id of its own; one that does
-- not ask has none, which is the whole of what `_if_assigned` reports
select txid_current() > 0;
select txid_current_if_assigned() is null;

create table xact_ids (label text, id xid8);

-- inside a block the id is assigned once and every later statement sees it
begin;
insert into xact_ids values ('block', pg_current_xact_id());
select txid_current() = txid_current() as stable_within_one_statement;
select txid_current()::text = txid_current_if_assigned()::text as assigned_now;
select pg_current_xact_id() = id as stable_across_statements
  from xact_ids where label = 'block';
-- both spellings report the same number, one as int8 and one as xid8
select txid_current()::text = pg_current_xact_id()::text as same_number;
select pg_xact_status(pg_current_xact_id()) as own_status;
commit;

-- and once the block ends, that same id reads as committed
select pg_xact_status(id) from xact_ids where label = 'block';

-- the same question under autocommit, where the id belongs to this one
-- statement: it is still running while its own rows are produced, so the answer
-- is not the `committed` a result set read after the commit would report
select pg_xact_status(pg_current_xact_id()) as own_status_autocommit;

-- assigning an id is not a write, so a read-only block may still ask for one
begin read only;
select txid_current() > 0 as allowed_in_a_read_only_block;
commit;

-- only a statement that asks consumes an id: the two plain reads between these
-- two lines consume none, so the difference is exactly one
insert into xact_ids values ('before reads', pg_current_xact_id());
select 1;
select 1;
select pg_current_xact_id()::text::int8 - id::text::int8 as ids_consumed
  from xact_ids where label = 'before reads';

-- pg_xact_status edges: the invalid id names no transaction, the two reserved
-- ids below the first normal one are permanently committed, and an id at or
-- above the next one to hand out has not started yet
select pg_xact_status('0'::xid8) is null as invalid_has_no_status;
select pg_xact_status('1'::xid8), pg_xact_status('2'::xid8);
select pg_xact_status(null::xid8) is null;
select pg_xact_status('18446744073709551615'::xid8);

drop table xact_ids;
