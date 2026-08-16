//! Total-order comparison of two `Value`s of a known `PgType` — the ordering
//! `ORDER BY`, a btree index, and every planner estimate all have to agree on.
//!
//! It lives here rather than in the executor because three crates need it and
//! only two of them may depend on execution: `crabgresql-pg-engine` sorts a
//! sample to build column statistics, `crabgresql-planner` walks a histogram to
//! estimate selectivity, and `crabgresql-executor` evaluates comparisons. The
//! executor re-exports these names, so its callers see no change.
//!
//! The value accessors below are the crate's one place for "this `Value` is
//! known to be of this type": they panic on a mismatch, which is an internal
//! invariant break (types were settled at bind time), never user input.

use std::cmp::Ordering;

use crate::collation::DEFAULT_COLLATION_OID;
use crate::{
    Inet, Interval, Numeric, PgType, TimeTz, Value, VectorKind, bit, collation, date, float,
    interval, json, money, net, time, timetz, tsquery, tsvector,
};

/// Total-order comparison of two non-null values of type `ty` under the
/// database's default collation. Floats use PG's total order (NaN sorts
/// greatest, `NaN = NaN`), so this also drives ORDER BY.
///
/// String comparison here is byte order. Use [`compare_values_collated`] where a
/// collation has been derived — comparison operators and ORDER BY — and this one
/// where the collation is provably irrelevant: equality and hashing (every
/// supported collation is deterministic, so equal bytes and equal values
/// coincide), and ordering of non-string types.
pub fn compare_values(ty: PgType, l: &Value, r: &Value) -> Ordering {
    compare_values_collated(ty, l, r, DEFAULT_COLLATION_OID)
}

/// Total-order comparison of two non-null values of type `ty`, ordering strings
/// under `collation`. Identical to [`compare_values`] for every other type.
pub fn compare_values_collated(ty: PgType, l: &Value, r: &Value, collation: u32) -> Ordering {
    match ty {
        PgType::Int2 => int2(l).cmp(&int2(r)),
        PgType::Int4 => int4(l).cmp(&int4(r)),
        PgType::Int8 => int8(l).cmp(&int8(r)),
        PgType::Float4 => float::f4_cmp(float4(l), float4(r)),
        PgType::Float8 => float::f8_cmp(float8(l), float8(r)),
        // Collation-driven comparison — byte order for `C`/`POSIX`/the database
        // default, the locale's order for an ICU collation. varchar and name
        // compare like text; bpchar ignores trailing blanks.
        PgType::Text | PgType::Varchar | PgType::Name => {
            collation::compare_str(collation, text(l), text(r))
        }
        PgType::Bpchar => collation::compare_str(
            collation,
            text(l).trim_end_matches(' '),
            text(r).trim_end_matches(' '),
        ),
        // `"char"` is a byte, not a string: no collation, and deliberately
        // **unsigned**, so `'\377' > 'a'`. PG's `btcharcmp` casts to `uint8` for
        // exactly this reason. Note the asymmetry with the `int4` conversion,
        // which reads the same byte as signed — see `crabgresql_types::char`.
        PgType::Char => char_of(l).cmp(&char_of(r)),
        PgType::Bytea => bytea(l).cmp(bytea(r)),
        // false < true, as in PG.
        PgType::Bool => bool_of(l).cmp(&bool_of(r)),
        // Microsecond order; the ±infinity sentinels sort naturally.
        PgType::Timestamp => timestamp_of(l).cmp(&timestamp_of(r)),
        PgType::TimestampTz => timestamptz_of(l).cmp(&timestamptz_of(r)),
        // Canonical-span order (30-day months, 24-hour days), infinities first/last.
        PgType::Interval => interval::cmp(interval_of(l), interval_of(r)),
        // Arbitrary-precision total order; NaN sorts greatest (== itself).
        PgType::Numeric => numeric(l).cmp(numeric(r)),
        // Day order (the ±infinity sentinels sort naturally); microsecond order;
        // UTC-instant-then-zone order.
        PgType::Date => date::cmp(date_of(l), date_of(r)),
        PgType::Time => time::cmp(time_of(l), time_of(r)),
        PgType::TimeTz => timetz::cmp(timetz_of(l), timetz_of(r)),
        // uuid: raw byte order (PG's `uuid_cmp`).
        PgType::Uuid => uuid_of(l).cmp(uuid_of(r)),
        // inet/cidr: family, common-prefix bits, masklen, address (`network_cmp`).
        PgType::Inet | PgType::Cidr => net::network_cmp(inet_of(l), inet_of(r)),
        // money: the natural i64 (cents) order.
        PgType::Money => money::cmp(money_of(l), money_of(r)),
        // oid: unsigned 32-bit order (PG's `oidcmp`).
        PgType::Oid => oid_of(l).cmp(&oid_of(r)),
        // tid: block first, then offset — PG's `tidcmp`, and the order the
        // heap itself lays rows out in.
        PgType::Tid => tid_of(l).cmp(&tid_of(r)),
        // Both transaction id types order as plain unsigned integers. `xid` is
        // reachable here only through equality and hashing — `is_orderable`
        // above keeps it out of every sort — but the arm must exist, because
        // `keys_equal` routes grouping equality through `compare_values`.
        PgType::Xid => xid_of(l).cmp(&xid_of(r)),
        PgType::Xid8 => xid8_of(l).cmp(&xid8_of(r)),
        // Only reached through `=`: `cid` has no btree opclass (see
        // `has_default_btree_opclass`), so nothing orders by it.
        PgType::Cid => cid_of(l).cmp(&cid_of(r)),
        // pg_lsn: the natural unsigned order of the 64-bit counter.
        PgType::PgLsn => lsn_of(l).cmp(&lsn_of(r)),
        // A reg* value orders by OID, never by the name it renders as — the
        // same rule its `PartialEq` and `hash_key` use.
        PgType::Reg(_) => reg_oid(l).cmp(&reg_oid(r)),
        // bit/varbit: common-prefix bit order, then shorter first (`bit_cmp`).
        PgType::Bit | PgType::Varbit => {
            let (la, da) = bit_of(l);
            let (lb, db) = bit_of(r);
            bit::cmp(la, da, lb, db)
        }
        // macaddr/macaddr8: raw byte order (PG's `macaddr_cmp`).
        PgType::Macaddr | PgType::Macaddr8 => macaddr_bytes(l).cmp(macaddr_bytes(r)),
        // jsonb: PG's `compareJsonbContainers` total order. (`json` has no
        // default ordering and never reaches here.)
        PgType::Jsonb => json::cmp(jsonb_of(l), jsonb_of(r)),
        // The text-search types carry their own total orders.
        PgType::Tsvector => tsvector::cmp(tsvector_of(l), tsvector_of(r)),
        PgType::Tsquery => tsquery::cmp(tsquery_of(l), tsquery_of(r)),
        // Arrays: element-wise comparison, then the shorter array is less on a
        // common prefix (PG's `array_cmp`). A NULL element sorts after any
        // non-NULL (NULLS-LAST), matching the default btree order.
        PgType::Array(elem_oid) => {
            let elem = PgType::from_oid(elem_oid).expect("orderable array element type resolves");
            compare_elementwise(elem, array_elems(l), array_elems(r))
        }
        // `oidvector` is the one type whose *sort* order is not its element-wise
        // order: PG gives it its own operator class (`btoidvectorcmp`), which
        // compares the element **count** first, so `'2' < '1 1'` is true.
        // `int2vector` has no opclass of its own and falls back to the
        // polymorphic array ordering, so for it `'2' > '1 1'`.
        //
        // This is the *btree* order — what ORDER BY, `<` and indexes use.
        // `min`/`max` deliberately do NOT use it; see the executor's
        // `compare_values_for_aggregate`, which routes around this arm.
        PgType::Vector(kind) => {
            let (la, lb) = (vector_elems(l), vector_elems(r));
            if matches!(kind, VectorKind::Oid) && la.len() != lb.len() {
                return la.len().cmp(&lb.len());
            }
            compare_elementwise(kind.element(), la, lb)
        }
        // Enums are the only user type with a query-time ordering (by
        // definition ordinal), which matches PG: a `CREATE TYPE` base type has
        // no default btree opclass, so ORDER BY on one fails with "could not
        // identify an ordering operator" — the binder's `is_orderable` admits
        // `PgType::User` for an enum only.
        // Keep this total for defensive callers: malformed/mixed values use
        // their actual non-user representation or type OID, never an unchecked
        // NULL unwrap or recursive redispatch through `PgType::User`.
        PgType::User(_) => match (l, r) {
            (
                Value::Enum {
                    type_oid: a_ty,
                    ordinal: a,
                    ..
                },
                Value::Enum {
                    type_oid: b_ty,
                    ordinal: b,
                    ..
                },
            ) => a_ty.cmp(b_ty).then_with(|| a.cmp(b)),
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            _ => match (l.pg_type(), r.pg_type()) {
                (Some(a), Some(b)) if a == b && !matches!(a, PgType::User(_)) => {
                    compare_values(a, l, r)
                }
                (Some(a), Some(b)) => a.oid().cmp(&b.oid()),
                _ => Ordering::Equal,
            },
        },
        other => unreachable!("comparison not supported for {other:?}"),
    }
}

pub fn int2(v: &Value) -> i16 {
    match v {
        Value::Int2(v) => *v,
        other => unreachable!("expected int2, got {other:?}"),
    }
}

pub fn array_elems(v: &Value) -> &[Value] {
    match v {
        Value::Array { elems, .. } => elems,
        other => unreachable!("expected array, got {other:?}"),
    }
}

/// Compare two element sequences the way PG's `array_cmp` does: element-wise,
/// then the shorter one first on a common prefix. A NULL element sorts after
/// any non-NULL (NULLS-LAST), matching the default btree order; vectors never
/// contain NULLs, so that arm is only reachable from arrays.
pub fn compare_elementwise(elem: PgType, la: &[Value], lb: &[Value]) -> Ordering {
    for (x, y) in la.iter().zip(lb.iter()) {
        let ord = match (x, y) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            _ => compare_values(elem, x, y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    la.len().cmp(&lb.len())
}

pub fn vector_elems(v: &Value) -> &[Value] {
    match v {
        Value::Vector { elems, .. } => elems,
        other => unreachable!("expected vector, got {other:?}"),
    }
}

pub fn int4(v: &Value) -> i32 {
    match v {
        Value::Int4(v) => *v,
        other => unreachable!("expected int4, got {other:?}"),
    }
}

pub fn oid_of(v: &Value) -> u32 {
    match v {
        Value::Oid(v) => *v,
        other => unreachable!("expected oid, got {other:?}"),
    }
}

pub fn tid_of(v: &Value) -> (u32, u16) {
    match v {
        Value::Tid { block, offset } => (*block, *offset),
        other => unreachable!("expected tid, got {other:?}"),
    }
}

pub fn xid_of(v: &Value) -> u32 {
    match v {
        Value::Xid(x) => *x,
        other => unreachable!("expected xid, got {other:?}"),
    }
}

pub fn cid_of(v: &Value) -> u32 {
    match v {
        Value::Cid(x) => *x,
        other => unreachable!("expected cid, got {other:?}"),
    }
}

pub fn xid8_of(v: &Value) -> u64 {
    match v {
        Value::Xid8(x) => *x,
        other => unreachable!("expected xid8, got {other:?}"),
    }
}

pub fn lsn_of(v: &Value) -> u64 {
    match v {
        Value::PgLsn(x) => *x,
        other => unreachable!("expected pg_lsn, got {other:?}"),
    }
}

pub fn reg_oid(v: &Value) -> u32 {
    match v {
        Value::Reg(r) => r.oid,
        other => unreachable!("expected a reg* value, got {other:?}"),
    }
}

pub fn int8(v: &Value) -> i64 {
    match v {
        Value::Int8(v) => *v,
        other => unreachable!("expected int8, got {other:?}"),
    }
}

pub fn float4(v: &Value) -> f32 {
    match v {
        Value::Float4(v) => *v,
        other => unreachable!("expected float4, got {other:?}"),
    }
}

pub fn float8(v: &Value) -> f64 {
    match v {
        Value::Float8(v) => *v,
        other => unreachable!("expected float8, got {other:?}"),
    }
}

pub fn numeric(v: &Value) -> &Numeric {
    match v {
        Value::Numeric(n) => n,
        other => unreachable!("expected numeric, got {other:?}"),
    }
}

pub fn money_of(v: &Value) -> i64 {
    match v {
        Value::Money(c) => *c,
        other => unreachable!("expected money, got {other:?}"),
    }
}

pub fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        other => unreachable!("expected text, got {other:?}"),
    }
}

pub fn bytea(v: &Value) -> &[u8] {
    match v {
        Value::Bytea(b) => b,
        other => unreachable!("expected bytea, got {other:?}"),
    }
}

pub fn bool_of(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        other => unreachable!("expected bool, got {other:?}"),
    }
}

pub fn char_of(v: &Value) -> u8 {
    match v {
        Value::Char(c) => *c,
        other => unreachable!("expected \"char\", got {other:?}"),
    }
}

pub fn uuid_of(v: &Value) -> &[u8; 16] {
    match v {
        Value::Uuid(b) => b,
        other => unreachable!("expected uuid, got {other:?}"),
    }
}

pub fn inet_of(v: &Value) -> &Inet {
    match v {
        Value::Inet(i) | Value::Cidr(i) => i,
        other => unreachable!("expected inet/cidr, got {other:?}"),
    }
}

pub fn bit_of(v: &Value) -> (u32, &[u8]) {
    match v {
        Value::Bit { len, data } => (*len, data),
        other => unreachable!("expected bit, got {other:?}"),
    }
}

pub fn macaddr_bytes(v: &Value) -> &[u8] {
    match v {
        Value::Macaddr(b) => b,
        Value::Macaddr8(b) => b,
        other => unreachable!("expected macaddr/macaddr8, got {other:?}"),
    }
}

pub fn jsonb_of(v: &Value) -> &json::Jsonb {
    match v {
        Value::Jsonb(j) => j,
        other => unreachable!("expected jsonb, got {other:?}"),
    }
}

pub fn tsvector_of(v: &Value) -> &tsvector::TsVector {
    match v {
        Value::Tsvector(t) => t,
        other => unreachable!("expected tsvector, got {other:?}"),
    }
}

pub fn tsquery_of(v: &Value) -> &tsquery::TsQuery {
    match v {
        Value::Tsquery(q) => q,
        other => unreachable!("expected tsquery, got {other:?}"),
    }
}

pub fn timestamp_of(v: &Value) -> i64 {
    match v {
        Value::Timestamp(t) => *t,
        other => unreachable!("expected timestamp, got {other:?}"),
    }
}

pub fn interval_of(v: &Value) -> Interval {
    match v {
        Value::Interval(iv) => *iv,
        other => unreachable!("expected interval, got {other:?}"),
    }
}

pub fn timestamptz_of(v: &Value) -> i64 {
    match v {
        Value::TimestampTz(t) => *t,
        other => unreachable!("expected timestamptz, got {other:?}"),
    }
}

pub fn date_of(v: &Value) -> i32 {
    match v {
        Value::Date(d) => *d,
        other => unreachable!("expected date, got {other:?}"),
    }
}

pub fn time_of(v: &Value) -> i64 {
    match v {
        Value::Time(t) => *t,
        other => unreachable!("expected time, got {other:?}"),
    }
}

pub fn timetz_of(v: &Value) -> TimeTz {
    match v {
        Value::TimeTz(t) => *t,
        other => unreachable!("expected timetz, got {other:?}"),
    }
}
#[cfg(test)]
mod vector_cmp_tests {
    use super::compare_values;
    use crate::{PgType, Value, VectorKind};
    use std::cmp::Ordering;

    fn v(kind: VectorKind, elems: &[i64]) -> Value {
        Value::Vector {
            kind,
            elems: elems
                .iter()
                .map(|n| match kind {
                    VectorKind::Oid => Value::Oid(*n as u32),
                    VectorKind::Int2 => Value::Int2(*n as i16),
                })
                .collect(),
        }
    }

    /// `oidvector` has its own operator class and compares the element count
    /// before any element; `int2vector` has none and compares element-wise.
    /// Both probed against PostgreSQL 18.4 — `'2' < '1 1'` is true for
    /// `oidvector` and false for `int2vector`.
    #[test]
    fn the_two_kinds_order_differently_on_unequal_lengths() {
        let (oid, int2) = (VectorKind::Oid, VectorKind::Int2);
        let ov = PgType::Vector(oid);
        let iv = PgType::Vector(int2);

        assert_eq!(
            compare_values(ov, &v(oid, &[2]), &v(oid, &[1, 1])),
            Ordering::Less
        );
        assert_eq!(
            compare_values(iv, &v(int2, &[2]), &v(int2, &[1, 1])),
            Ordering::Greater
        );
        assert_eq!(
            compare_values(ov, &v(oid, &[1, 5]), &v(oid, &[1, 1, 1])),
            Ordering::Less
        );
        assert_eq!(
            compare_values(iv, &v(int2, &[9, 8]), &v(int2, &[1, 1, 1])),
            Ordering::Greater
        );
    }

    /// At equal length both kinds compare element-wise, and a shorter prefix
    /// still sorts first — `'1' < '1 2'` either way.
    #[test]
    fn equal_lengths_and_common_prefixes_agree() {
        for kind in [VectorKind::Oid, VectorKind::Int2] {
            let ty = PgType::Vector(kind);
            assert_eq!(
                compare_values(ty, &v(kind, &[2, 0]), &v(kind, &[1, 9])),
                Ordering::Greater,
                "{kind:?}"
            );
            assert_eq!(
                compare_values(ty, &v(kind, &[1]), &v(kind, &[1, 2])),
                Ordering::Less,
                "{kind:?}"
            );
            assert_eq!(
                compare_values(ty, &v(kind, &[1, 2]), &v(kind, &[1, 2])),
                Ordering::Equal,
                "{kind:?}"
            );
        }
    }
}

#[cfg(test)]
mod enum_cmp_tests {
    use super::compare_values;
    use crate::{PgType, Value};
    use std::cmp::Ordering;

    fn e(ordinal: u32, label: &str) -> Value {
        Value::Enum {
            type_oid: 16384,
            ordinal,
            label: label.into(),
        }
    }

    #[test]
    fn enum_orders_by_definition_ordinal_not_label() {
        let ty = PgType::User(16384);
        // 'red'(0) < 'green'(3), even though "green" < "red" alphabetically.
        assert_eq!(
            compare_values(ty, &e(0, "red"), &e(3, "green")),
            Ordering::Less
        );
        assert_eq!(
            compare_values(ty, &e(3, "green"), &e(0, "red")),
            Ordering::Greater
        );
        assert_eq!(
            compare_values(ty, &e(2, "yellow"), &e(2, "yellow")),
            Ordering::Equal
        );
    }

    #[test]
    fn malformed_user_comparisons_are_total() {
        let ty = PgType::User(16384);
        assert_eq!(
            compare_values(ty, &Value::Null, &e(0, "red")),
            Ordering::Less
        );
        assert_eq!(
            compare_values(ty, &e(0, "red"), &Value::Int4(1)),
            Ordering::Greater
        );
        assert_eq!(
            compare_values(
                ty,
                &e(0, "red"),
                &Value::Enum {
                    type_oid: 16385,
                    ordinal: 0,
                    label: "other".into(),
                },
            ),
            Ordering::Less
        );
    }
}
