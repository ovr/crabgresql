//! AST `DataType` → [`PgType`], and the type modifiers a declaration carries:
//! extraction (`length_typmod`, `numeric_typmod`, …) and the application of one
//! to an expression or a value.

use std::sync::Arc;

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{Column, TypeCatalog};
use crabgresql_types::{Numeric, PgType, RegKind, Value, time, timestamp};

use crate::BindError;
use crate::functions::ScalarFn;

use super::bound::BoundExpr;
use super::literal::literal_int;
use super::scope::normalize_ident;

/// Types the executor's `compare_values` can order. Both comparison operators
/// (`=`, `<`, …) and ORDER BY require this — binding a sort or comparison on any
/// other type would produce a node the evaluator can't handle.
pub(crate) fn is_orderable(ty: PgType, catalog: &dyn TypeCatalog) -> bool {
    // An array is orderable/comparable iff its element type is (element-wise
    // comparison). Keep in sync with the executor's `compare_values`.
    if let PgType::Array(elem_oid) = ty {
        return PgType::from_oid(elem_oid).is_some_and(|e| is_orderable(e, catalog));
    }
    matches!(
        ty,
        PgType::Bool
            | PgType::Bit
            | PgType::Varbit
            | PgType::Int2
            | PgType::Int4
            | PgType::Int8
            | PgType::Float4
            | PgType::Float8
            | PgType::Numeric
            | PgType::Text
            | PgType::Varchar
            | PgType::Bpchar
            // `"char"` has a default btree opclass (`btcharcmp`); it orders
            // *unsigned*, which `compare_values` implements.
            | PgType::Char
            | PgType::Name
            | PgType::Oid
            | PgType::Tid
            // `xid8` is an ordinary unsigned counter and orders normally.
            // `xid` is deliberately absent — see `has_equality` below.
            | PgType::Xid8
            | PgType::PgLsn
            // A reg* value compares by the OID it holds, never by the name it
            // renders as — see `compare_values` in the executor.
            | PgType::Reg(_)
            | PgType::Bytea
            | PgType::Date
            | PgType::Time
            | PgType::TimeTz
            | PgType::Timestamp
            | PgType::Interval
            | PgType::TimestampTz
            | PgType::Uuid
            | PgType::Inet
            | PgType::Cidr
            | PgType::Money
            | PgType::Macaddr
            | PgType::Macaddr8
            // `jsonb` has a total order (`compareJsonbContainers`); plain `json`
            // has no default equality/ordering, so it is intentionally omitted.
            | PgType::Jsonb
            // Both text-search types have a default btree opclass in PG.
            | PgType::Tsvector
            | PgType::Tsquery
            // Both vectors are orderable, but by different rules — `oidvector`
            // via its own `btoidvectorcmp` opclass (element count first),
            // `int2vector` via the polymorphic array ordering (element-wise).
            // See `compare_values_collated`. Their elements are always
            // `oid`/`int2`, both orderable, so there is no element check here.
            | PgType::Vector(_)
    ) || matches!(ty, PgType::User(oid) if catalog.enum_info(oid).is_some())
        // A domain orders exactly as its base does: the value under it *is* a
        // base value, and `compare_values` dispatches on the value. `base_type`
        // answering something else is itself the "this is a domain" test, so
        // there is no second lookup to ask it.
        || (catalog.base_type(ty) != ty && is_orderable(catalog.base_type(ty), catalog))
}

/// Types with a default *equality* operator — a superset of the orderable ones,
/// and the right gate for `=`/`<>` and for every dedup (GROUP BY, DISTINCT,
/// UNION), which need equality but never an ordering.
///
/// `xid` and `cid` are the only types in the gap. PostgreSQL gives each a hash
/// operator class but deliberately no btree one, because transaction and command
/// ids compare with modular arithmetic — so `'1'::xid = '1'::xid` binds while
/// `'1'::xid < '2'::xid` must still fail with `operator does not exist`, and
/// ORDER BY / `min` / `max` on either must fail too. Those all stay gated on
/// [`is_orderable`].
///
/// This admits `<>` on both, which is right for `xid` and one operator too many
/// for `cid` — upstream gives `cid` only `=`. That last step is applied where
/// the operator is resolved rather than here, because this predicate also gates
/// GROUP BY / DISTINCT / UNION, and `cid` does belong in all three.
pub(crate) fn has_equality(ty: PgType, catalog: &dyn TypeCatalog) -> bool {
    if let PgType::Array(elem) = ty {
        return PgType::from_oid(elem).is_some_and(|e| has_equality(e, catalog));
    }
    let ty = catalog.base_type(ty);
    matches!(ty, PgType::Xid | PgType::Cid) || is_orderable(ty, catalog)
}

/// The built-in a type *spelling* denotes under PG's type-name grammar, or
/// `None` if it names no built-in.
///
/// This is the resolution `regtypein`, a `CAST` target and a `CREATE TYPE ...
/// LIKE` target all use, and it is not the same as [`PgType::from_name`], which
/// maps a catalog `typname`. Quoting is what separates them: unquoted `char` is
/// the `char(1)` keyword (`bpchar`), while quoted `"char"` is the one-byte type
/// (oid 18). Any caller holding user-written syntax must come through here,
/// since a bare `&str` has already lost the quotes.
pub fn builtin_type_from_syntax(s: &str) -> Option<PgType> {
    let dt = crabgresql_parser::parse_data_type(s).ok()?;
    map_data_type(&dt).ok()
}

/// Map a SQL type name to a `PgType`. Shared by cast/typed-string binding and
/// server-side CREATE TABLE.
pub fn map_data_type(dt: &ast::DataType) -> Result<PgType, BindError> {
    use ast::DataType;
    Ok(match dt {
        DataType::Bool | DataType::Boolean => PgType::Bool,
        DataType::SmallInt(_) | DataType::Int2(_) => PgType::Int2,
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => PgType::Int4,
        DataType::BigInt(_) | DataType::Int8(_) => PgType::Int8,
        DataType::Real | DataType::Float4 => PgType::Float4,
        DataType::DoublePrecision | DataType::Float8 => PgType::Float8,
        DataType::Double(_) => PgType::Float8,
        // float(p): p <= 24 is single precision, else double (PG semantics).
        DataType::Float(info) => match precision_of(info) {
            Some(p) if p <= 24 => PgType::Float4,
            _ => PgType::Float8,
        },
        DataType::Numeric(_) | DataType::Decimal(_) => PgType::Numeric,
        DataType::Bytea => PgType::Bytea,
        DataType::Date => PgType::Date,
        // `time` / `time with time zone`. The precision modifier is not part of
        // the type itself; `datetime_precision` reads it back out of the name
        // and the coercion rounds to it.
        DataType::Time(_, tz) => match tz {
            ast::TimezoneInfo::None | ast::TimezoneInfo::WithoutTimeZone => PgType::Time,
            ast::TimezoneInfo::WithTimeZone | ast::TimezoneInfo::Tz => PgType::TimeTz,
        },
        // `timestamp` / `timestamp with time zone`; the precision modifier is
        // read separately, as for `time` above.
        DataType::Timestamp(_, tz) => match tz {
            ast::TimezoneInfo::None | ast::TimezoneInfo::WithoutTimeZone => PgType::Timestamp,
            ast::TimezoneInfo::WithTimeZone | ast::TimezoneInfo::Tz => PgType::TimestampTz,
        },
        // `interval`; the field qualifier and the precision are read back out
        // of the name by `interval_typmod`, which packs both into one modifier.
        DataType::Interval { .. } => PgType::Interval,
        DataType::Uuid => PgType::Uuid,
        DataType::Inet => PgType::Inet,
        DataType::Cidr => PgType::Cidr,
        DataType::Text => PgType::Text,
        // `varchar`/`character varying` (with or without a length limit).
        DataType::Varchar(_) | DataType::CharacterVarying(_) => PgType::Varchar,
        // `char`/`character` (blank-padded; a bare `char` is `char(1)`).
        DataType::Char(_) | DataType::Character(_) => PgType::Bpchar,
        // `bit(n)` (fixed) and `bit varying(n)` / `varbit` (variable); the length
        // is enforced separately as a typmod coercion.
        DataType::Bit(_) => PgType::Bit,
        DataType::BitVarying(_) | DataType::VarBit(_) => PgType::Varbit,
        DataType::JSON => PgType::Json,
        DataType::JSONB => PgType::Jsonb,
        // The parser has dedicated keyword variants for these two; the bareword
        // arm below still catches the schema-qualified spellings.
        DataType::TsVector => PgType::Tsvector,
        DataType::TsQuery => PgType::Tsquery,
        DataType::Regclass => PgType::Reg(RegKind::Class),
        // Geometric types — the whole family is modeled.
        DataType::GeometricType(kind) => match kind {
            ast::GeometricTypeKind::Point => PgType::Point,
            ast::GeometricTypeKind::LineSegment => PgType::Lseg,
            ast::GeometricTypeKind::GeometricPath => PgType::Path,
            ast::GeometricTypeKind::GeometricBox => PgType::Box,
            ast::GeometricTypeKind::Polygon => PgType::Polygon,
            ast::GeometricTypeKind::Line => PgType::Line,
            ast::GeometricTypeKind::Circle => PgType::Circle,
        },
        // `T[]` / `ARRAY[N]` / `ARRAY<T>`: an array of the element type. The
        // `int[5]` length is accepted and ignored (PG does not enforce array
        // length), and so is *depth*: `int[][]` names the same type as `int[]`,
        // because an array's dimensionality belongs to the value, not the type.
        // A bare `ARRAY` (no element type) has no meaning here.
        DataType::Array(elem_def) => {
            let inner = match elem_def {
                ast::ArrayElemTypeDef::SquareBracket(inner, _)
                | ast::ArrayElemTypeDef::AngleBracket(inner)
                | ast::ArrayElemTypeDef::Parenthesis(inner) => inner,
                ast::ArrayElemTypeDef::None => {
                    return Err(BindError::feature_not_supported(
                        "array type without an element type is not supported",
                    ));
                }
            };
            // A nested `DataType::Array` is one more `[]` in the declaration, so
            // peel it off to reach the type that is actually the element. Only
            // the *syntax* peels: `int4[][]` is `integer[]`, but `_int4[]` names
            // an array of an array type, which PG has no such thing as and the
            // check below refuses.
            let inner_ty = map_data_type(inner)?;
            let elem = match inner_ty {
                PgType::Array(elem_oid) if matches!(inner.as_ref(), DataType::Array(_)) => {
                    PgType::from_oid(elem_oid).ok_or_else(|| {
                        BindError::feature_not_supported(format!(
                            "type \"{dt}\" is not supported yet"
                        ))
                    })?
                }
                elem => elem,
            };
            // Only element types this build has an array type for are supported;
            // this is what rejects an array element type reached by name.
            if crabgresql_types::array::array_oid_for_elem(elem.oid()).is_none() {
                return Err(BindError::feature_not_supported(format!(
                    "type \"{dt}\" is not supported yet"
                )));
            }
            PgType::Array(elem.oid())
        }
        // Type names the parser has no dedicated `DataType` for arrive here:
        // `bpchar`, `name`, `point`, and every built-in written with a
        // `pg_catalog.` qualifier. A modifier rides along in `mods` rather than
        // in a typed field, so `bpchar(4)` and `pg_catalog.varchar(4)` name the
        // same types as `char(4)` and `varchar(4)`; the three typmod readers
        // below pick the length back out.
        DataType::Custom(obj, _) => match builtin_custom_type(obj) {
            Some(t) => t,
            None => {
                return Err(BindError::feature_not_supported(format!(
                    "type \"{dt}\" is not supported yet"
                )));
            }
        },
        other => {
            return Err(BindError::feature_not_supported(format!(
                "type \"{other}\" is not supported yet"
            )));
        }
    })
}

/// The built-in a `DataType::Custom` name denotes, or `None` if it names no
/// built-in — an unknown type, or one qualified with a schema other than
/// `pg_catalog`. Both fall through to the user-type lookup in [`bind_cast`].
///
/// Built-ins live in `pg_catalog`, so a bare `int4` and `pg_catalog.int4` name
/// the same type while `app.int4` names a user type that merely shares the
/// spelling. psql leans on the qualified form throughout `\d` (`::pg_catalog.text`,
/// `pr.prattrs::pg_catalog.int2[]`), which `DataType::Array` picks up by
/// recursing through here.
pub(crate) fn builtin_custom_type(obj: &ast::ObjectName) -> Option<PgType> {
    let parts = obj
        .0
        .iter()
        .map(|p| p.as_ident().map(normalize_ident))
        .collect::<Option<Vec<_>>>()?;
    let name = match parts.as_slice() {
        [name] => name.as_str(),
        [schema, name] if schema == "pg_catalog" => name.as_str(),
        _ => return None,
    };
    PgType::from_name(name)
}

fn precision_of(info: &ast::ExactNumberInfo) -> Option<u64> {
    match info {
        ast::ExactNumberInfo::None => None,
        ast::ExactNumberInfo::Precision(p) => Some(*p),
        ast::ExactNumberInfo::PrecisionAndScale(p, _) => Some(*p),
    }
}

/// The *last* part of a non-built-in type name — what the column a cast to it
/// produces is called, since PostgreSQL's `FigureColname` never qualifies:
/// `'YES'::information_schema.yes_or_no` heads its column `yes_or_no`.
/// Distinct from [`custom_type_key`], which is what the catalog is asked.
pub(super) fn custom_type_name(dt: &ast::DataType) -> Option<String> {
    match dt {
        ast::DataType::Custom(obj, mods) if mods.is_empty() => {
            obj.0.last().and_then(|p| p.as_ident()).map(normalize_ident)
        }
        _ => None,
    }
}

/// The key a written non-built-in type name is looked up by, or `None` for a
/// `DataType` that is not one.
///
/// A qualifier that is *not* a search path — anything but `public`, where every
/// `CREATE TYPE` lands, and `pg_catalog`, which [`builtin_custom_type`] has
/// already had its chance at — travels with the name, because such a type
/// answers only to its qualified spelling. That is what PostgreSQL does:
/// `information_schema` is not on the search path, so `'x'::sql_identifier`
/// raises 42704 there while `'x'::information_schema.sql_identifier` resolves.
/// [`crabgresql_catalog::SystemCatalog::user_type_oid`] draws the same line, and
/// dropping the qualifier here is what used to let the two disagree.
pub fn custom_type_key(dt: &ast::DataType) -> Option<String> {
    let ast::DataType::Custom(obj, mods) = dt else {
        return None;
    };
    if !mods.is_empty() {
        return None;
    }
    let parts = obj
        .0
        .iter()
        .map(|p| p.as_ident().map(normalize_ident))
        .collect::<Option<Vec<_>>>()?;
    match parts.as_slice() {
        [name] => Some(name.clone()),
        [schema, name] if schema == "public" || schema == "pg_catalog" => Some(name.clone()),
        [schema, name] => Some(format!("{schema}.{name}")),
        _ => None,
    }
}

/// Resolve a written type name to a [`PgType`], falling back to the catalog for
/// `CREATE TYPE` names. This is the resolution a cast target goes through, so a
/// type name that works in `expr::t` works everywhere this is used — notably a
/// PL/pgSQL variable declaration, whose type is lifted out of the routine body
/// as text.
pub fn resolve_data_type(
    catalog: &Arc<dyn TypeCatalog>,
    data_type: &ast::DataType,
) -> Result<PgType, BindError> {
    match map_data_type(data_type) {
        Ok(t) => Ok(t),
        // Not a builtin type name — it may be a `CREATE TYPE` name; resolve it
        // against the catalog, else surface the original "not supported" error.
        Err(e) => match custom_type_key(data_type).and_then(|n| catalog.resolve_type(&n)) {
            Some(ut) => Ok(PgType::User(ut.oid)),
            None => Err(e),
        },
    }
}

/// The `(precision, scale)` of a `numeric(p[,s])` / `decimal(...)` type name,
/// or `None` for an unconstrained `numeric`. A bare `numeric(p)` has scale 0.
pub(crate) fn numeric_typmod(dt: &ast::DataType) -> Option<(i32, i32)> {
    use ast::{DataType, ExactNumberInfo};
    let info = match dt {
        DataType::Numeric(i) | DataType::Decimal(i) => i,
        // `pg_catalog.numeric(5,2)`: the modifier arrives as raw token text.
        DataType::Custom(obj, mods) => {
            if builtin_custom_type(obj) != Some(PgType::Numeric) {
                return None;
            }
            let modifier = |s: &String| literal_int(s).and_then(|v| i32::try_from(v).ok());
            let precision = modifier(mods.first()?)?;
            let scale = mods.get(1).map_or(Some(0), modifier)?;
            return Some((precision, scale));
        }
        _ => return None,
    };
    match info {
        ExactNumberInfo::None => None,
        ExactNumberInfo::Precision(p) => Some((*p as i32, 0)),
        ExactNumberInfo::PrecisionAndScale(p, s) => Some((*p as i32, *s as i32)),
    }
}

/// [`numeric_typmod`] with PostgreSQL's declared-precision bounds enforced, for
/// the same reason [`checked_length_typmod`] exists: `numeric(0)` and
/// `numeric(1001)` are not merely odd, PostgreSQL rejects them outright while
/// resolving the type name — before any value is looked at.
pub fn checked_numeric_typmod(dt: &ast::DataType) -> Result<Option<(i32, i32)>, BindError> {
    let Some((precision, scale)) = numeric_typmod(dt) else {
        return Ok(None);
    };
    let invalid = |message: String| BindError::new(sqlstate::INVALID_PARAMETER_VALUE, message);
    if !(1..=1000).contains(&precision) {
        return Err(invalid(format!(
            "NUMERIC precision {precision} must be between 1 and 1000"
        )));
    }
    if !(-1000..=1000).contains(&scale) {
        return Err(invalid(format!(
            "NUMERIC scale {scale} must be between -1000 and 1000"
        )));
    }
    Ok(Some((precision, scale)))
}

/// When `target` is `numeric` and `data_type` carries a `(p,s)` modifier, apply
/// it — folding constants at bind time (so overflow errors here, with PG's
/// DETAIL) and inserting a runtime length-coercion for non-constants.
pub(super) fn apply_numeric_typmod_if_any(
    expr: BoundExpr,
    target: PgType,
    data_type: &ast::DataType,
) -> Result<BoundExpr, BindError> {
    if target != PgType::Numeric {
        return Ok(expr);
    }
    let Some((precision, scale)) = checked_numeric_typmod(data_type)? else {
        return Ok(expr);
    };
    apply_numeric_typmod(expr, precision, scale)
}

/// Round `expr` to a `numeric(precision, scale)`, folding a constant at bind time
/// and emitting a runtime coercion otherwise.
///
/// PostgreSQL applies the same rounding in cast and assignment context — both
/// round to the scale and both raise `22003` when the integer part no longer
/// fits — so unlike the character and bit types this needs no
/// truncate-vs-error flag, and the cast path ([`apply_numeric_typmod_if_any`])
/// and the column path ([`apply_length_to_column`]) share it.
pub(super) fn apply_numeric_typmod(
    expr: BoundExpr,
    precision: i32,
    scale: i32,
) -> Result<BoundExpr, BindError> {
    if let BoundExpr::Const {
        value: Value::Numeric(n),
        ..
    } = &expr
    {
        let applied = n
            .apply_typmod(precision, scale)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail))?;
        return Ok(BoundExpr::Const {
            value: Value::Numeric(applied),
            ty: PgType::Numeric,
        });
    }
    Ok(BoundExpr::FuncCall {
        func: ScalarFn::NumApplyTypmod,
        ret: PgType::Numeric,
        args: vec![
            expr,
            BoundExpr::Const {
                value: Value::Int4(precision),
                ty: PgType::Int4,
            },
            BoundExpr::Const {
                value: Value::Int4(scale),
                ty: PgType::Int4,
            },
        ],
    })
}

/// The declared character length of a `char(n)`/`varchar(n)` type name. A bare
/// `char`/`character` defaults to length 1; a bare `varchar` has no limit.
pub fn length_typmod(dt: &ast::DataType) -> Option<i32> {
    use ast::DataType;
    fn len(l: &Option<ast::CharacterLength>) -> Option<i32> {
        match l {
            Some(ast::CharacterLength::IntegerLength { length, .. }) => Some(*length as i32),
            _ => None,
        }
    }
    match dt {
        DataType::Char(l) | DataType::Character(l) => Some(len(l).unwrap_or(1)),
        DataType::Varchar(l) | DataType::CharacterVarying(l) => len(l),
        // `bit(n)` defaults to `bit(1)`; `bit varying` with no length is unlimited.
        DataType::Bit(n) => Some(n.map(|n| n as i32).unwrap_or(1)),
        DataType::BitVarying(n) | DataType::VarBit(n) => n.map(|n| n as i32),
        // `bpchar(4)` / `pg_catalog.varchar(4)`: same types, modifier as raw
        // token text. A modifier-less `bpchar` is unlimited (unlike a bare
        // `char`, which the grammar defaults to `char(1)`), so no `unwrap_or`.
        DataType::Custom(obj, mods) => match builtin_custom_type(obj) {
            Some(PgType::Bpchar | PgType::Varchar | PgType::Bit | PgType::Varbit) => {
                literal_int(mods.first()?).and_then(|v| i32::try_from(v).ok())
            }
            _ => None,
        },
        _ => None,
    }
}

/// The largest `character`/`character varying` length PostgreSQL accepts, and
/// the largest `bit`/`bit varying` one. Probed against 18.4, which rejects both
/// ends of the range (`varchar(0)` and `varchar(10485761)`) with 22023.
const MAX_CHAR_LENGTH: u64 = 10_485_760;
const MAX_BIT_LENGTH: u64 = 83_886_080;

/// [`length_typmod`] with PostgreSQL's declared-length bounds enforced.
///
/// Worth having separately from `length_typmod`, which cannot report an error:
/// an unchecked length is not merely accepted-but-odd, it reaches
/// `pg_attribute` as a stored `atttypmod`, where PostgreSQL's `n + VARHDRSZ`
/// encoding would overflow `i32` and take down every later reader of the
/// catalog. Rejecting the DDL is also what PostgreSQL does, and the error names
/// the type by its `typname` (`char`, not `character`), as PG's does.
pub fn checked_length_typmod(dt: &ast::DataType) -> Result<Option<i32>, BindError> {
    use ast::DataType;
    fn declared(l: &Option<ast::CharacterLength>) -> Option<u64> {
        match l {
            Some(ast::CharacterLength::IntegerLength { length, .. }) => Some(*length),
            _ => None,
        }
    }
    let (length, typname, max) = match dt {
        DataType::Char(l) | DataType::Character(l) => (declared(l), "char", MAX_CHAR_LENGTH),
        DataType::Varchar(l) | DataType::CharacterVarying(l) => {
            (declared(l), "varchar", MAX_CHAR_LENGTH)
        }
        DataType::Bit(n) => (*n, "bit", MAX_BIT_LENGTH),
        DataType::BitVarying(n) | DataType::VarBit(n) => (*n, "varbit", MAX_BIT_LENGTH),
        // The `bpchar(4)` / `pg_catalog.varchar(4)` spellings, whose modifier
        // is raw token text; the bound and the `typname` in the error follow
        // the type the name resolves to.
        DataType::Custom(obj, mods) => {
            let declared = mods
                .first()
                .and_then(|m| literal_int(m))
                .and_then(|v| u64::try_from(v).ok());
            match builtin_custom_type(obj) {
                Some(PgType::Bpchar) => (declared, "char", MAX_CHAR_LENGTH),
                Some(PgType::Varchar) => (declared, "varchar", MAX_CHAR_LENGTH),
                Some(PgType::Bit) => (declared, "bit", MAX_BIT_LENGTH),
                Some(PgType::Varbit) => (declared, "varbit", MAX_BIT_LENGTH),
                // `"char"` takes no modifier at all, and PG rejects one rather
                // than ignoring it — so this cannot fall through to `Ok(None)`
                // the way a genuinely modifier-less type does.
                Some(PgType::Char) if !mods.is_empty() => {
                    return Err(BindError::new(
                        sqlstate::SYNTAX_ERROR,
                        "type modifier is not allowed for type \"char\"".to_string(),
                    ));
                }
                _ => return Ok(None),
            }
        }
        // No other type carries a length modifier here.
        _ => return Ok(None),
    };
    if let Some(n) = length {
        let invalid = |message: String| BindError::new(sqlstate::INVALID_PARAMETER_VALUE, message);
        if n < 1 {
            return Err(invalid(format!(
                "length for type {typname} must be at least 1"
            )));
        }
        if n > max {
            return Err(invalid(format!(
                "length for type {typname} cannot exceed {max}"
            )));
        }
    }
    Ok(length_typmod(dt))
}

/// The fractional-second precision of a `time(p)`/`timestamp(p)` type name,
/// clamped to what any datetime type can hold.
///
/// PostgreSQL warns and clamps rather than rejecting a larger modifier
/// (`TIMESTAMP(7) precision reduced to maximum allowed, 6`), and since the
/// clamped value is what it then stores, the clamping matches. A negative
/// modifier never reaches this: the grammar rejects it.
///
/// TODO: emit the `precision reduced to maximum allowed` WARNING that PG
/// raises alongside the clamp.
pub fn datetime_precision(dt: &ast::DataType) -> Option<i32> {
    use ast::DataType;
    let p = match dt {
        DataType::Time(p, _) | DataType::Timestamp(p, _) => (*p)?,
        _ => return None,
    };
    Some((p as i32).min(crabgresql_types::timestamp::MAX_PRECISION))
}

/// The packed modifier of an `interval` type name — the admitted fields and the
/// fractional-second precision in one `i32`, as `pg_attribute.atttypmod` stores
/// them. `None` for a bare `interval`, which has no modifier at all.
///
/// Like the other datetime types, a precision above 6 is clamped (PostgreSQL
/// also warns; see [`datetime_precision`]). The grammar already rejects a
/// precision on a range that does not reach `SECOND`, so that combination cannot
/// arrive here.
pub fn interval_typmod(dt: &ast::DataType) -> Option<i32> {
    use ast::{DataType, IntervalFields as F};
    use crabgresql_types::interval as iv;

    let DataType::Interval { fields, precision } = dt else {
        return None;
    };
    let range = match fields {
        None => iv::FULL_RANGE,
        Some(F::Year) => iv::MASK_YEAR,
        Some(F::Month) => iv::MASK_MONTH,
        Some(F::Day) => iv::MASK_DAY,
        Some(F::Hour) => iv::MASK_HOUR,
        Some(F::Minute) => iv::MASK_MINUTE,
        Some(F::Second) => iv::MASK_SECOND,
        Some(F::YearToMonth) => iv::MASK_YEAR | iv::MASK_MONTH,
        Some(F::DayToHour) => iv::MASK_DAY | iv::MASK_HOUR,
        Some(F::DayToMinute) => iv::MASK_DAY | iv::MASK_HOUR | iv::MASK_MINUTE,
        Some(F::DayToSecond) => iv::MASK_DAY | iv::MASK_HOUR | iv::MASK_MINUTE | iv::MASK_SECOND,
        Some(F::HourToMinute) => iv::MASK_HOUR | iv::MASK_MINUTE,
        Some(F::HourToSecond) => iv::MASK_HOUR | iv::MASK_MINUTE | iv::MASK_SECOND,
        Some(F::MinuteToSecond) => iv::MASK_MINUTE | iv::MASK_SECOND,
    };
    // `interval` with neither fields nor precision carries no modifier, exactly
    // as an undecorated column does.
    if fields.is_none() && precision.is_none() {
        return None;
    }
    let p = precision.map(|p| (p as i32).min(crabgresql_types::timestamp::MAX_PRECISION) as u8);
    Some(iv::pack_typmod(range, p))
}

/// The type modifier a written-out type name declares, in the raw encoding
/// [`crabgresql_storage_api::Column::typmod`] uses. `None` when the name carries
/// no modifier.
///
/// These are the *checked* readers, not the bare ones: an out-of-range modifier
/// would otherwise be stored on a column and later overflow
/// `pg_attribute.atttypmod`. `numeric` packs two numbers into the one slot;
/// `interval` packs its admitted fields alongside the precision; every other
/// modifier is a bare length or fractional-second precision.
pub fn declared_typmod(ty: PgType, dt: &ast::DataType) -> Result<Option<i32>, BindError> {
    Ok(match ty {
        PgType::Numeric => checked_numeric_typmod(dt)?.map(|(p, s)| Numeric::pack_typmod(p, s)),
        PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
            datetime_precision(dt)
        }
        PgType::Interval => interval_typmod(dt),
        _ => checked_length_typmod(dt)?,
    })
}

/// Coerce `expr` to an `interval` modifier, folding a constant at bind time.
/// Cast and assignment context coerce identically, so this serves both.
///
/// Folding can fail the way the runtime call would: rounding a `usec` at the
/// `i64` extreme to a declared precision is "interval out of range".
pub(super) fn apply_interval_typmod(expr: BoundExpr, typmod: i32) -> Result<BoundExpr, BindError> {
    if let BoundExpr::Const {
        value: Value::Interval(iv),
        ty,
    } = &expr
    {
        let iv = crabgresql_types::interval::apply_typmod(*iv, typmod)
            .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        return Ok(BoundExpr::Const {
            value: Value::Interval(iv),
            ty: *ty,
        });
    }
    Ok(BoundExpr::FuncCall {
        func: ScalarFn::IntervalTypmod,
        ret: PgType::Interval,
        args: vec![
            expr,
            BoundExpr::Const {
                value: Value::Int4(typmod),
                ty: PgType::Int4,
            },
        ],
    })
}

/// Round `expr` to a datetime type's fractional-second precision, folding a
/// constant at bind time. Cast and assignment context round identically, so this
/// serves both.
pub(crate) fn apply_datetime_precision(
    expr: BoundExpr,
    precision: i32,
) -> Result<BoundExpr, BindError> {
    let rounded = match &expr {
        BoundExpr::Const { value, ty } => match value {
            Value::Time(usec) => Some(Value::Time(time::apply_typmod(*usec, precision))),
            Value::TimeTz(t) => Some(Value::TimeTz(crabgresql_types::TimeTz {
                usec: time::apply_typmod(t.usec, precision),
                zone: t.zone,
            })),
            Value::Timestamp(usec) => {
                Some(Value::Timestamp(timestamp::apply_typmod(*usec, precision)))
            }
            Value::TimestampTz(usec) => Some(Value::TimestampTz(timestamp::apply_typmod(
                *usec, precision,
            ))),
            // NULL, or a constant of some other type that a coercion above will
            // still convert.
            _ => None,
        }
        .map(|value| BoundExpr::Const { value, ty: *ty }),
        _ => None,
    };
    Ok(rounded.unwrap_or_else(|| BoundExpr::FuncCall {
        func: ScalarFn::TimeApplyTypmod,
        ret: expr.ty(),
        args: vec![
            expr,
            BoundExpr::Const {
                value: Value::Int4(precision),
                ty: PgType::Int4,
            },
        ],
    }))
}

/// Apply a `varchar(n)`/`char(n)` length coercion, or a `name` truncation, when
/// the target is one of those types. Constant inputs fold at bind time.
pub(super) fn apply_length_typmod_if_any(
    expr: BoundExpr,
    target: PgType,
    data_type: &ast::DataType,
) -> Result<BoundExpr, BindError> {
    let (func, typmod) = match target {
        PgType::Varchar => match checked_length_typmod(data_type)? {
            Some(n) => (ScalarFn::VarcharTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Bpchar => match checked_length_typmod(data_type)? {
            Some(n) => (ScalarFn::BpcharTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Bit => match checked_length_typmod(data_type)? {
            Some(n) => (ScalarFn::BitTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Varbit => match checked_length_typmod(data_type)? {
            Some(n) => (ScalarFn::VarbitTypmod, Some(n)),
            None => return Ok(expr),
        },
        // `name` always clips to 63 bytes, independent of any modifier.
        PgType::Name => (ScalarFn::NameInput, None),
        PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
            match datetime_precision(data_type) {
                Some(p) => return apply_datetime_precision(expr, p),
                None => return Ok(expr),
            }
        }
        PgType::Interval => match interval_typmod(data_type) {
            Some(m) => return apply_interval_typmod(expr, m),
            None => return Ok(expr),
        },
        // `"char"` takes no modifier and PG rejects one rather than ignoring
        // it, so this target still has to run the check even though it never
        // yields a typmod — the DDL path reaches `checked_length_typmod`
        // directly, but a cast only gets here.
        PgType::Char => {
            checked_length_typmod(data_type)?;
            return Ok(expr);
        }
        _ => return Ok(expr),
    };
    // Fold a constant value now (explicit-cast semantics: truncate/pad).
    if let BoundExpr::Const {
        value: Value::Text(s),
        ..
    } = &expr
    {
        let folded = match func {
            ScalarFn::VarcharTypmod => {
                let Some(typmod) = typmod else {
                    return Err(BindError::new("XX000", "varchar typmod is missing"));
                };
                crabgresql_types::text::truncate_chars(s, typmod)
            }
            ScalarFn::BpcharTypmod => {
                let Some(typmod) = typmod else {
                    return Err(BindError::new("XX000", "bpchar typmod is missing"));
                };
                crabgresql_types::text::bpchar_input(s, typmod, true)
                    .map_err(|e| BindError::new(e.sqlstate, e.message))?
            }
            ScalarFn::NameInput => crabgresql_types::text::name_input(s),
            _ => unreachable!(),
        };
        return Ok(BoundExpr::Const {
            value: Value::Text(folded),
            ty: target,
        });
    }
    if let BoundExpr::Const {
        value: Value::Bit { len, data },
        ..
    } = &expr
    {
        let Some(typmod) = typmod else {
            return Err(BindError::new("XX000", "bit typmod is missing"));
        };
        let (len, data) =
            crabgresql_types::bit::coerce(*len, data, typmod, target == PgType::Varbit, true)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        return Ok(BoundExpr::Const {
            value: Value::Bit { len, data },
            ty: target,
        });
    }
    let mut args = vec![expr];
    if let Some(n) = typmod {
        args.push(BoundExpr::Const {
            value: Value::Int4(n),
            ty: PgType::Int4,
        });
    }
    Ok(BoundExpr::FuncCall {
        func,
        ret: target,
        args,
    })
}

/// Apply a column's `varchar(n)`/`char(n)`/`name` length coercion in assignment
/// context (an over-long varchar/char errors unless the excess is blank).
pub(super) fn apply_length_to_column(
    expr: BoundExpr,
    column: &Column,
) -> Result<BoundExpr, BindError> {
    apply_length(expr, column.ty, column.typmod)
}

/// [`apply_length_to_column`] over a bare `(type, typmod)` pair, which is how a
/// domain carries its modifier: `CREATE DOMAIN v AS varchar(3)` stores the 3 on
/// the *type*, and a column of that domain has `atttypmod = -1`.
pub(super) fn apply_length(
    expr: BoundExpr,
    ty: PgType,
    typmod: i32,
) -> Result<BoundExpr, BindError> {
    apply_length_in(expr, ty, typmod, false)
}

/// [`apply_length`] in **explicit-cast** context, where an over-long
/// varchar/char truncates instead of raising. The distinction is observable on
/// a domain, which is the only place one `(type, typmod)` pair is reached from
/// both contexts: `'abcd'::v3` yields `abc` while inserting `'abcd'` into a `v3`
/// column raises `value too long for type character varying(3)`.
pub(super) fn apply_length_cast(
    expr: BoundExpr,
    ty: PgType,
    typmod: i32,
) -> Result<BoundExpr, BindError> {
    apply_length_in(expr, ty, typmod, true)
}

fn apply_length_in(
    expr: BoundExpr,
    ty: PgType,
    typmod: i32,
    explicit: bool,
) -> Result<BoundExpr, BindError> {
    // `numeric` and the datetime types round rather than truncating, and share
    // their whole implementation with the cast path.
    if typmod >= 0 {
        match ty {
            PgType::Numeric => {
                let (precision, scale) = Numeric::unpack_typmod(typmod);
                return apply_numeric_typmod(expr, precision, scale);
            }
            PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
                return apply_datetime_precision(expr, typmod);
            }
            PgType::Interval => return apply_interval_typmod(expr, typmod),
            _ => {}
        }
    }
    let func = match ty {
        PgType::Varchar if typmod >= 0 => ScalarFn::VarcharTypmod,
        PgType::Bpchar if typmod >= 0 => ScalarFn::BpcharTypmod,
        PgType::Bit if typmod >= 0 => ScalarFn::BitTypmod,
        PgType::Varbit if typmod >= 0 => ScalarFn::VarbitTypmod,
        PgType::Name => ScalarFn::NameInput,
        _ => return Ok(expr),
    };
    // Fold a constant now, under whichever of the two rules applies.
    if let BoundExpr::Const {
        value: Value::Text(s),
        ..
    } = &expr
    {
        let folded = match (func, explicit) {
            (ScalarFn::VarcharTypmod, true) => crabgresql_types::text::truncate_chars(s, typmod),
            (ScalarFn::VarcharTypmod, false) => {
                crabgresql_types::text::varchar_input(s, typmod, false)
                    .map_err(|e| BindError::new(e.sqlstate, e.message))?
            }
            (ScalarFn::BpcharTypmod, _) => {
                crabgresql_types::text::bpchar_input(s, typmod, explicit)
                    .map_err(|e| BindError::new(e.sqlstate, e.message))?
            }
            (ScalarFn::NameInput, _) => crabgresql_types::text::name_input(s),
            _ => unreachable!(),
        };
        return Ok(BoundExpr::Const {
            value: Value::Text(folded),
            ty,
        });
    }
    if let BoundExpr::Const {
        value: Value::Bit { len, data },
        ..
    } = &expr
    {
        let (len, data) =
            crabgresql_types::bit::coerce(*len, data, typmod, ty == PgType::Varbit, explicit)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        return Ok(BoundExpr::Const {
            value: Value::Bit { len, data },
            ty,
        });
    }
    let mut args = vec![expr];
    if func != ScalarFn::NameInput {
        args.push(BoundExpr::Const {
            value: Value::Int4(typmod),
            ty: PgType::Int4,
        });
        // Third arg: 0 = assignment (error on overflow), 1 = truncating cast.
        args.push(BoundExpr::Const {
            value: Value::Int4(explicit as i32),
            ty: PgType::Int4,
        });
    }
    Ok(BoundExpr::FuncCall {
        func,
        ret: ty,
        args,
    })
}

/// [`apply_length_to_column`] for a value that is already in hand.
///
/// COPY parses each field straight into its column's [`Value`], so there is no
/// expression for the folding half of `apply_length_to_column` to look inside.
/// Both call the same value-level cores — `apply_typmod`, `varchar_input`,
/// `bpchar_input`, `name_input`, `bit::coerce` — so this is a second caller of
/// the length rules, not a second copy of them.
///
/// `false` to the input functions is assignment context: an over-long
/// `varchar(n)`/`char(n)` whose excess is not blank errors rather than
/// truncating.
pub fn apply_typmod_value(value: Value, ty: PgType, typmod: i32) -> Result<Value, BindError> {
    // A NULL takes no length. On the expression path this falls out of the fold
    // guards and reaches the runtime call, which returns NULL; here it is said
    // once, up front.
    if matches!(value, Value::Null) {
        return Ok(value);
    }
    // `parse_unknown` produced this value *for* `ty`, so a shape that
    // does not match the type is a bug in this file rather than bad input.
    let mismatch = || {
        BindError::new(
            sqlstate::INTERNAL_ERROR,
            format!("copy value does not match column type {}", ty.name()),
        )
    };
    // `numeric` and the datetime types round rather than truncating; each arm
    // returns, so the length pass below only sees the types it handles.
    if typmod >= 0 {
        match ty {
            PgType::Numeric => {
                let Value::Numeric(n) = value else {
                    return Err(mismatch());
                };
                let (precision, scale) = Numeric::unpack_typmod(typmod);
                return n
                    .apply_typmod(precision, scale)
                    .map(Value::Numeric)
                    .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail));
            }
            PgType::Time => {
                let Value::Time(usec) = value else {
                    return Err(mismatch());
                };
                return Ok(Value::Time(time::apply_typmod(usec, typmod)));
            }
            PgType::TimeTz => {
                let Value::TimeTz(t) = value else {
                    return Err(mismatch());
                };
                return Ok(Value::TimeTz(crabgresql_types::TimeTz {
                    usec: time::apply_typmod(t.usec, typmod),
                    zone: t.zone,
                }));
            }
            PgType::Timestamp => {
                let Value::Timestamp(usec) = value else {
                    return Err(mismatch());
                };
                return Ok(Value::Timestamp(timestamp::apply_typmod(usec, typmod)));
            }
            PgType::TimestampTz => {
                let Value::TimestampTz(usec) = value else {
                    return Err(mismatch());
                };
                return Ok(Value::TimestampTz(timestamp::apply_typmod(usec, typmod)));
            }
            PgType::Interval => {
                let Value::Interval(iv) = value else {
                    return Err(mismatch());
                };
                return crabgresql_types::interval::apply_typmod(iv, typmod)
                    .map(Value::Interval)
                    .map_err(|e| BindError::new(e.sqlstate, e.message));
            }
            _ => {}
        }
    }
    // `name` truncates whatever its typmod says, and a `name` column's is always
    // -1 — hence the unconditional arm, exactly as in `apply_length_to_column`.
    match ty {
        // The text family's length rules read the *text*, not the `Value`, so
        // they live in `text_value` — where COPY reaches them without
        // building a `String` first. A length-free `text`/`varchar` matches
        // neither arm and falls through untouched, keeping its allocation.
        PgType::Varchar | PgType::Bpchar if typmod >= 0 => {
            let Value::Text(s) = value else {
                return Err(mismatch());
            };
            text_value(&s, ty, typmod)
        }
        PgType::Name => {
            let Value::Text(s) = value else {
                return Err(mismatch());
            };
            text_value(&s, ty, typmod)
        }
        PgType::Bit | PgType::Varbit if typmod >= 0 => {
            let Value::Bit { len, data } = value else {
                return Err(mismatch());
            };
            crabgresql_types::bit::coerce(len, &data, typmod, ty == PgType::Varbit, false)
                .map(|(len, data)| Value::Bit { len, data })
                .map_err(|e| BindError::new(e.sqlstate, e.message))
        }
        _ => Ok(value),
    }
}

/// A text-family column's value for the field text `s`, built with exactly one
/// allocation: the length rule is applied to the *slice*, not to a `String` that
/// was allocated only to be read once and dropped.
///
/// This is COPY's route into the text family. The expression path reaches the
/// same rules through [`apply_typmod_value`], which unwraps its `Value::Text`
/// and lands here, so `varchar(n)`'s `22001`, `char(n)`'s blank padding and
/// `name`'s byte clip have one implementation rather than two.
///
/// `false` to the input functions is assignment context, as in
/// [`apply_typmod_value`]: an over-long value whose excess is not blank errors
/// rather than truncating.
pub fn text_value(s: &str, ty: PgType, typmod: i32) -> Result<Value, BindError> {
    match ty {
        PgType::Varchar if typmod >= 0 => crabgresql_types::text::varchar_input(s, typmod, false)
            .map(Value::Text)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Bpchar if typmod >= 0 => crabgresql_types::text::bpchar_input(s, typmod, false)
            .map(Value::Text)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // `name` clips at 63 bytes whatever its typmod says (a `name` column's
        // is always -1), matching the arm above in `apply_typmod_value`.
        PgType::Name => Ok(Value::Text(crabgresql_types::text::name_input(s))),
        // `text`, and a `varchar`/`char` with no declared length: no rule to
        // apply, so the field text is the value.
        _ => Ok(Value::Text(s.to_string())),
    }
}
