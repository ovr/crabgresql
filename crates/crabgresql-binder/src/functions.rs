//! Scalar function resolution.
//!
//! Clean-room (see AGENTS.md): the function set, argument coercions, and error
//! text reproduce PG's *observable* behavior for the functions the float
//! regression tests call, pinned by the corpus. A minimal name+arity+coercion
//! resolver stands in for PG's full overload machinery — enough for these
//! tests, where arguments are floats, unknown literals, or ints promoted to
//! float8.

use std::sync::Arc;

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{RoutineImpl, RoutineKind, RoutineSig, TypeCatalog};
use crabgresql_types::{PgType, RegKind};

use crate::expr::{
    Binding, BoundExpr, Scope, bind_expr, bind_sql_function_body, coerce_for_arg, inline_params,
};
use crate::{BindError, OutputColumn};

/// A scalar function the executor can evaluate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarFn {
    Trunc,
    Round,
    Ceil,
    Floor,
    Sign,
    Sqrt,
    Cbrt,
    Exp,
    Ln,
    Power,
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    Erf,
    Erfc,
    Gamma,
    Lgamma,
    Sind,
    Cosd,
    Tand,
    Cotd,
    Asind,
    Acosd,
    Atand,
    Atan2d,
    Float4Send,
    Float8Send,
    PgInputIsValid,
    Md5,
    /// `date_part(text, timestamp) -> float8`.
    DatePart,
    /// `EXTRACT(field FROM timestamp) -> numeric`; the field is a text arg.
    Extract,
    /// `date_trunc(text, timestamp) -> timestamp`.
    DateTrunc,
    /// `isfinite(timestamp) -> bool`.
    Isfinite,
    /// `make_timestamp(int, int, int, int, int, float8) -> timestamp`.
    MakeTimestamp,

    // --- inet/cidr operators (built directly by the binder) ---
    /// `a << b` — `a` is contained by `b` (`->` bool).
    NetworkContainedBy,
    /// `a >> b` — `a` contains `b` (`->` bool).
    NetworkContains,
    /// `a && b` — the networks overlap (`->` bool).
    NetworkOverlaps,
    /// `inet & inet -> inet` (`inetand`).
    InetAnd,
    /// `inet | inet -> inet` (`inetor`).
    InetOr,
    /// `~inet -> inet` (`inetnot`).
    InetNot,
    /// `inet + int8 -> inet` (`inetpl`).
    InetPlInt8,
    /// `inet - int8 -> inet` (`inetmi_int8`).
    InetMiInt8,
    /// `inet - inet -> int8` (`inetmi`).
    InetMi,

    // --- inet/cidr functions ---
    /// `host(inet) -> text`.
    Host,
    /// `masklen(inet) -> int4`.
    Masklen,
    /// `family(inet) -> int4`.
    Family,
    /// `network(inet) -> cidr`.
    Network,
    /// `abbrev(inet) -> text`.
    AbbrevInet,
    /// `abbrev(cidr) -> text`.
    AbbrevCidr,

    // --- macaddr / macaddr8 operators + functions (width-dispatched at
    // runtime; the result type is carried in the `BoundExpr`). ---
    /// `~macaddr` / `~macaddr8` — one's complement.
    MacaddrNot,
    /// `macaddr & macaddr` / `macaddr8 & macaddr8` — bytewise AND.
    MacaddrAnd,
    /// `macaddr | macaddr` / `macaddr8 | macaddr8` — bytewise OR.
    MacaddrOr,
    /// `trunc(macaddr)` / `trunc(macaddr8)` — zero the low bytes.
    MacaddrTrunc,
    /// `macaddr8_set7bit(macaddr8) -> macaddr8`.
    Macaddr8Set7bit,

    // --- interval operators (built directly by the binder, not via `lookup`) ---
    /// unary `- interval`.
    IntervalNeg,
    /// `interval + interval`.
    IntervalPl,
    /// `interval - interval`.
    IntervalMi,
    /// `interval * float8` (factor is arg 1).
    IntervalMul,
    /// `interval / float8`.
    IntervalDiv,
    /// `timestamp + interval -> timestamp`.
    TimestampPlInterval,
    /// `timestamp - interval -> timestamp`.
    TimestampMiInterval,
    /// `timestamp - timestamp -> interval`.
    TimestampMi,

    // --- interval functions ---
    /// `date_part(text, interval) -> float8`.
    DatePartInterval,
    /// `EXTRACT(field FROM interval) -> numeric`.
    ExtractInterval,
    /// `date_trunc(text, interval) -> interval`.
    DateTruncInterval,
    /// `isfinite(interval) -> bool`.
    IsfiniteInterval,
    /// `make_interval(int, int, int, int, int, int, float8) -> interval`.
    MakeInterval,
    /// `justify_days(interval) -> interval`.
    JustifyDays,
    /// `justify_hours(interval) -> interval`.
    JustifyHours,
    /// `justify_interval(interval) -> interval`.
    JustifyInterval,
    /// `age(timestamp, timestamp) -> interval`.
    Age,
    /// `to_char(interval, text) -> text`.
    ToCharInterval,

    // --- timestamptz operators/functions ---
    /// `date_part(text, timestamptz) -> float8`.
    DatePartTz,
    /// `EXTRACT(field FROM timestamptz) -> numeric`; the field is a text arg.
    ExtractTz,
    /// `date_trunc(text, timestamptz) -> timestamptz`.
    DateTruncTz,
    /// `isfinite(timestamptz) -> bool`.
    IsfiniteTz,
    /// `make_timestamptz(int×5, float8[, text]) -> timestamptz`.
    MakeTimestampTz,
    /// `timezone(text, timestamp) -> timestamptz` (`ts AT TIME ZONE zone`).
    TimezoneToTz,
    /// `timezone(text, timestamptz) -> timestamp` (`tstz AT TIME ZONE zone`).
    TimezoneToTs,

    // --- date operators/functions (built by the binder unless noted) ---
    /// `date + int4 -> date`.
    DatePlDays,
    /// `date - int4 -> date`.
    DateMiDays,
    /// `date - date -> int4`.
    DateMi,
    /// `date + interval -> timestamp`.
    DatePlInterval,
    /// `date - interval -> timestamp`.
    DateMiInterval,
    /// `date + time -> timestamp`.
    DatePlTime,
    /// `date + timetz -> timestamptz`.
    DatePlTimeTz,
    /// `date_part(text, date) -> float8`.
    DatePartDate,
    /// `EXTRACT(field FROM date) -> numeric`.
    ExtractDate,
    /// `isfinite(date) -> bool`.
    IsfiniteDate,
    /// `make_date(int, int, int) -> date`.
    MakeDate,

    // --- time operators/functions ---
    /// `time + interval -> time`.
    TimePlInterval,
    /// `time - interval -> time`.
    TimeMiInterval,
    /// `time - time -> interval`.
    TimeMi,
    /// `date_part(text, time) -> float8`.
    DatePartTime,
    /// `EXTRACT(field FROM time) -> numeric`.
    ExtractTime,
    /// `make_time(int, int, float8) -> time`.
    MakeTime,

    // --- timetz operators/functions ---
    /// `timetz + interval -> timetz`.
    TimeTzPlInterval,
    /// `timetz - interval -> timetz`.
    TimeTzMiInterval,
    /// `date_part(text, timetz) -> float8`.
    DatePartTimeTz,
    /// `EXTRACT(field FROM timetz) -> numeric`.
    ExtractTimeTz,

    // ---- numeric-typed math (arg and result are `numeric`) ----
    /// `round(numeric [, int4]) -> numeric` (round half away from zero).
    NumRound,
    /// `trunc(numeric [, int4]) -> numeric` (toward zero).
    NumTrunc,
    /// `ceil(numeric) -> numeric`.
    NumCeil,
    /// `floor(numeric) -> numeric`.
    NumFloor,
    /// `abs(numeric) -> numeric`.
    NumAbs,
    /// `sign(numeric) -> numeric`.
    NumSign,
    /// `mod(numeric, numeric) -> numeric`.
    NumMod,
    /// `sqrt(numeric) -> numeric`.
    NumSqrt,
    /// `ln(numeric) -> numeric`.
    NumLn,
    /// `log(numeric) -> numeric` (base 10).
    NumLog10,
    /// `log(numeric, numeric) -> numeric` (base, value).
    NumLog,
    /// `exp(numeric) -> numeric`.
    NumExp,
    /// `power(numeric, numeric) -> numeric`, also the `^` operator.
    NumPower,
    /// Apply a `numeric(precision, scale)` type modifier at run time. Args are
    /// `(numeric, int4 precision, int4 scale)`; the length coercion PG inserts
    /// for `x::numeric(p,s)`.
    NumApplyTypmod,
    /// `abs(float8) -> float8`.
    AbsF8,
    /// `log(float8) -> float8` (base 10).
    Log10F8,
    /// `mod(intN, intN) -> intN` (dispatches on the operand's integer width).
    ModInt,

    // --- string functions (see `crabgresql_types::text`) ----
    /// `text || text -> text` (the `||` operator / `textcat`).
    TextConcat,
    /// `length`/`char_length`/`character_length(text) -> int4`.
    Length,
    /// `octet_length(text) -> int4`.
    OctetLength,
    /// `bit_length(text) -> int4`.
    BitLength,
    /// `upper(text) -> text`.
    Upper,
    /// `lower(text) -> text`.
    Lower,
    /// `initcap(text) -> text`.
    Initcap,
    /// `substr`/`substring(text, int4 [, int4]) -> text`.
    Substr,
    /// `strpos(text, text) -> int4` (also `position(sub IN str)`).
    StrPos,
    /// `overlay(text, text, int4 [, int4]) -> text`.
    Overlay,
    /// `ltrim(text [, text]) -> text`.
    Ltrim,
    /// `rtrim(text [, text]) -> text`.
    Rtrim,
    /// `btrim(text [, text]) -> text` (also `trim(both ...)`).
    Btrim,
    /// `lpad(text, int4 [, text]) -> text`.
    Lpad,
    /// `rpad(text, int4 [, text]) -> text`.
    Rpad,
    /// `replace(text, text, text) -> text`.
    Replace,
    /// `translate(text, text, text) -> text`.
    Translate,
    /// `repeat(text, int4) -> text`.
    Repeat,
    /// `reverse(text) -> text`.
    Reverse,
    /// `left(text, int4) -> text`.
    Left,
    /// `right(text, int4) -> text`.
    Right,
    /// `ascii(text) -> int4`.
    Ascii,
    /// `chr(int4) -> text`.
    Chr,
    /// `split_part(text, text, int4) -> text`.
    SplitPart,
    /// `starts_with(text, text) -> bool`.
    StartsWith,
    /// `to_hex(int4) -> text`.
    ToHex,
    /// `to_hex(int8) -> text`.
    ToHexInt8,
    /// `concat(...) -> text` (variadic, non-strict: NULL args are skipped).
    Concat,
    /// `concat_ws(sep, ...) -> text` (variadic, non-strict; NULL sep -> NULL).
    ConcatWs,
    /// `format(text, ...) -> text` (variadic, non-strict).
    Format,
    /// `text LIKE text -> bool` (case-sensitive).
    Like,
    /// `text ILIKE text -> bool` (case-insensitive).
    ILike,
    /// `text ~ text -> bool` (POSIX regex, case-sensitive).
    RegexMatch,
    /// `text ~* text -> bool` (POSIX regex, case-insensitive).
    RegexIMatch,
    /// `text SIMILAR TO text [ESCAPE text] -> bool`.
    SimilarTo,
    /// `regexp_replace(source, pattern, replacement [, flags]) -> text`.
    RegexpReplace,
    /// `regexp_like(string, pattern [, flags]) -> bool`.
    RegexpLike,
    /// `regexp_count(string, pattern [, start [, flags]]) -> int4`.
    RegexpCount,
    /// `regexp_substr(string, pattern [, start [, n [, flags [, subexpr]]]]) -> text`.
    RegexpSubstr,
    /// `substring(text, text) -> text`: POSIX-regex extraction.
    SubstringRegex,
    /// `substring(text, text, text) -> text`: SQL-regex (`SIMILAR`) extraction.
    SubstringSimilar,
    /// `encode(bytea, text) -> text`.
    Encode,
    /// `decode(text, text) -> bytea`.
    Decode,
    /// `quote_ident(text) -> text`.
    QuoteIdent,
    /// `quote_literal(text) -> text`.
    QuoteLiteral,
    /// `quote_nullable(text) -> text`.
    QuoteNullable,
    /// Apply a `varchar(n)` length coercion at run time (`text`, `int4 n`).
    VarcharTypmod,
    /// Apply a `char(n)`/`bpchar(n)` blank-padding coercion (`text`, `int4 n`).
    BpcharTypmod,
    /// `name` input: truncate to 63 characters (`text`).
    NameInput,
    /// `bpchar -> text` coercion: strip trailing blanks.
    BpcharToText,

    // --- money operators/functions (built by the binder unless noted) ---
    /// unary `- money` (`cash_um`).
    CashUm,
    /// `money + money -> money`.
    CashPl,
    /// `money - money -> money`.
    CashMi,
    /// `money * intN -> money` / `intN * money -> money` (factor widened to int8).
    CashMulInt,
    /// `money * floatN -> money` / `floatN * money -> money` (factor as float8).
    CashMulFlt,
    /// `money / intN -> money` (integer division, truncating).
    CashDivInt,
    /// `money / floatN -> money` (float division, rounded).
    CashDivFlt,
    /// `money / money -> float8`.
    CashDivCash,
    /// `cash_words(money) -> text`.
    CashWords,
    /// `cashlarger(money, money) -> money`.
    CashLarger,
    /// `cashsmaller(money, money) -> money`.
    CashSmaller,
    // --- bit / varbit ---
    /// `~bit` (bitwise NOT).
    BitNot,
    /// `bit & bit` (bitwise AND); errors on differing sizes.
    BitAnd,
    /// `bit | bit` (bitwise OR); errors on differing sizes.
    BitOr,
    /// `bit # bit` (bitwise XOR); errors on differing sizes.
    BitXor,
    /// `bit || bit` (concatenation).
    BitConcat,
    /// `bit << int4` (shift left, keeping length).
    BitShl,
    /// `bit >> int4` (shift right, keeping length).
    BitShr,
    /// `length(bit) -> int4` (the number of bits).
    BitLen,
    /// `bit_count(bit) -> int8` (the number of set bits).
    BitCount,
    /// `get_bit(bit, int4) -> int4`.
    GetBit,
    /// `set_bit(bit, int4, int4) -> bit`.
    SetBit,
    /// `substring(bit, int4[, int4]) -> bit`.
    SubstrBit,
    /// `position(bit IN bit) -> int4` (`strpos(bit, bit)`).
    BitPosition,
    /// `overlay(bit, bit, int4[, int4]) -> bit`.
    OverlayBit,
    /// Apply a `bit(n)` length coercion (`bit`, `int4 n`[, `int4` explicit flag]).
    BitTypmod,
    /// Apply a `bit varying(n)` length coercion (`bit`, `int4 n`[, flag]).
    VarbitTypmod,
    // --- geometric (point / lseg) ---
    /// A geometric operator/function; the specific operation is the payload.
    Geo(GeoFn),
    // --- sequences (side-effecting; dispatched via the executor's SequenceOps,
    // not the pure `eval_scalar`) ---
    /// `nextval(regclass) -> int8`: advance a sequence and return its new value.
    Nextval,
    /// `currval(regclass) -> int8`: this session's last `nextval` for a sequence.
    Currval,
    /// `setval(regclass, int8[, bool]) -> int8`: set a sequence's counter.
    Setval,
    /// `lastval() -> int8`: this session's last `nextval`, for any sequence.
    Lastval,
    // --- catalog lookups (dispatched via the executor's CatalogOps, not the
    // pure `eval_scalar`, which has no view of the session's pg_catalog) ---
    /// `pg_get_userbyid(oid) -> name`: the role's name, or `unknown (OID=n)`.
    PgGetUserById,
    /// `pg_table_is_visible(oid) -> bool`: whether the relation is reachable by
    /// an unqualified name. NULL for an OID no relation has.
    PgTableIsVisible,
    /// `'name'::reg*`: resolve an object name to the OID it identifies, erroring
    /// if nothing has that name. Emitted by a cast, not callable by name — PG
    /// spells these `regclassin` and friends, which are not SQL-visible either.
    RegIn(RegKind),
    /// `oid::reg*`: take an OID as-is and resolve the name it renders as. An OID
    /// that names nothing is not an error (it prints as `-` or its digits), so
    /// this never fails.
    RegFromOid(RegKind),
    // --- catalog deparse / type formatting (dispatched by the executor's `eval`,
    // not the pure `eval_scalar`: `format_type` is non-strict in its typmod
    // argument, which `eval_scalar`'s STRICT short-circuit cannot express) ---
    /// `format_type(oid, int4) -> text`: PostgreSQL's SQL spelling of a type with
    /// its modifier applied (`character varying(20)`, `numeric(4,2)`). NULL oid
    /// yields NULL; a NULL modifier means "no modifier" (not NULL).
    FormatType,
    /// `pg_get_expr(text, oid[, bool]) -> text`: deparse a stored node tree back
    /// to SQL. crabgresql already stores canonical SQL text (column defaults,
    /// `relpartbound`), so this echoes its first argument.
    PgGetExpr,
    // --- jsonpath (jsonb @ jsonpath) ---
    /// A `jsonb_path_*` function / `@?` / `@@` operator. Args are
    /// `[jsonb, jsonpath]` optionally followed by `[vars jsonb, silent bool]`;
    /// the `@?`/`@@` operators pass a `silent = true` 4th arg.
    JsonPath(JsonPathFn),
    // --- text search (tsvector / tsquery) ---
    /// A `tsvector`/`tsquery` operation; the specific operation is the payload.
    Ts(TsFn),

    // --- array operators / functions (built directly by the binder; the result
    // type is carried in the `BoundExpr`). All operate on 1-D arrays. ---
    /// `array || array` / `array || element` (`arrcat`/`array_append`). Args are
    /// `[array, array]`.
    ArrayCat,
    /// `array_append(array, element)` and `array || element`. Args `[array, elem]`.
    ArrayAppend,
    /// `array_prepend(element, array)` and `element || array`. Args `[elem, array]`.
    ArrayPrepend,
    /// `array @> array` — the left array contains every element of the right.
    ArrayContains,
    /// `array <@ array` — the left array's elements are all in the right.
    ArrayContainedBy,
    /// `array && array` — the arrays share at least one element.
    ArrayOverlap,
    /// `array_length(array, dim int4) -> int4` (NULL for an empty array or a
    /// dimension other than 1).
    ArrayLength,
    /// `array_upper(array, dim int4) -> int4` — the upper subscript bound. For
    /// the 1-based 1-D arrays here this equals the length; NULL for an empty
    /// array or a dimension other than 1.
    ArrayUpper,
    /// `cardinality(array) -> int4` — the total number of elements.
    Cardinality,
    /// `array_to_string(array, delimiter text[, null_string text]) -> text`.
    /// Non-strict on the optional `null_string`: renders each element (NULLs
    /// skipped, or replaced by `null_string`) joined by the delimiter.
    ArrayToString,
}

/// A SQL/JSON path query entry point. All take a `jsonb` target and a `jsonpath`;
/// see [`ScalarFn::JsonPath`] for the argument convention.
///
/// The SQL functions (`Exists`/`Match`/`QueryArray`/`QueryFirst`) are STRICT and
/// read an optional `vars`/`silent`; the `@?`/`@@` operator variants
/// (`ExistsOp`/`MatchOp`) take exactly `[jsonb, jsonpath]` and always run in
/// silent mode — kept distinct so the STRICT functions don't have to encode
/// silence as a synthetic NULL argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonPathFn {
    /// `jsonb_path_exists(...)` (`-> boolean`, STRICT).
    Exists,
    /// `jsonb_path_match(...)` (`-> boolean`, three-valued, STRICT).
    Match,
    /// `jsonb_path_query_array(...)` (`-> jsonb`, the matches wrapped in an array).
    QueryArray,
    /// `jsonb_path_query_first(...)` (`-> jsonb`, the first match or NULL).
    QueryFirst,
    /// `jsonb @? jsonpath` (`-> boolean`; silent form of `Exists`).
    ExistsOp,
    /// `jsonb @@ jsonpath` (`-> boolean`; silent form of `Match`).
    MatchOp,
}

/// A text-search operation over `tsvector`/`tsquery`. Operators lower to these
/// via `resolve_ts_op` (`@@`, `&&`, `<->`), `resolve_ts_concat` (`||`) and
/// `resolve_ts_unary` (`!!`); named functions register them in [`lookup`].
/// Argument order is fixed per variant.
///
/// PG spells the weight arguments as `"char"` and `"char"[]`, which this engine
/// has no type for; they are modeled as `text`/`text[]`, so a literal like
/// `setweight(v, 'c')` binds identically. Only the first character is read, as
/// the `"char"` cast would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TsFn {
    /// `tsvector @@ tsquery` (`-> boolean`). Operand order is normalized, so the
    /// `tsquery @@ tsvector` spelling lowers here too.
    Match,
    /// `tsvector || tsvector` (`-> tsvector`).
    VectorConcat,
    /// `strip(tsvector) -> tsvector`.
    Strip,
    /// `length(tsvector) -> int4`.
    VectorLength,
    /// `setweight(tsvector, text) -> tsvector`.
    SetWeight,
    /// `setweight(tsvector, text, text[]) -> tsvector`.
    SetWeightLexemes,
    /// `ts_delete(tsvector, text)` / `ts_delete(tsvector, text[])` (`-> tsvector`).
    Delete,
    /// `ts_filter(tsvector, text[]) -> tsvector`.
    Filter,
    /// `tsvector_to_array(tsvector) -> text[]`.
    VectorToArray,
    /// `array_to_tsvector(text[]) -> tsvector`.
    ArrayToVector,
    /// `numnode(tsquery) -> int4`.
    NumNode,
    /// `querytree(tsquery) -> text`.
    QueryTree,
    /// `tsquery && tsquery` (`-> tsquery`).
    QueryAnd,
    /// `tsquery || tsquery` (`-> tsquery`).
    QueryOr,
    /// `!! tsquery` (`-> tsquery`).
    QueryNot,
    /// `tsquery <-> tsquery` and `tsquery_phrase(tsquery, tsquery)` (`-> tsquery`).
    QueryPhrase,
    /// `tsquery_phrase(tsquery, tsquery, int4) -> tsquery`.
    QueryPhraseDist,
}

/// A geometric (`point` / `lseg`) operation. Operators lower to these via
/// `resolve_geometric_op`/`resolve_geometric_unary`; named functions register
/// them in [`lookup`]. Argument order is fixed per variant (see each doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeoFn {
    /// `point(float8, float8) -> point`.
    PointConstruct,
    /// `p1 <-> p2` point distance (`-> float8`).
    PointDist,
    /// `p1 << p2` (strictly left).
    PointLeft,
    /// `p1 >> p2` (strictly right).
    PointRight,
    /// `p1 |>> p2` (strictly above).
    PointAbove,
    /// `p1 <<| p2` (strictly below).
    PointBelow,
    /// `p1 ~= p2` (same as).
    PointEq,
    /// `p1 ?- p2` / `ishorizontal(p1, p2)` (share a y).
    PointHoriz,
    /// `p1 ?| p2` / `isvertical(p1, p2)` (share an x).
    PointVert,
    /// `p1 + p2` translate (`-> point`).
    PointAdd,
    /// `p1 - p2` translate (`-> point`).
    PointSub,
    /// `p1 * p2` complex multiply (`-> point`).
    PointMul,
    /// `p1 / p2` complex divide (`-> point`).
    PointDiv,
    /// `slope(p1, p2) -> float8`.
    PointSlope,
    /// `point <-> lseg` distance (`-> float8`); args are `[point, lseg]`.
    DistPointSeg,
    /// `point <@ lseg` (point lies on the segment); args are `[point, lseg]`.
    PointOnSeg,
    /// `point ## lseg` closest point on the segment; args are `[point, lseg]`.
    ClosePointSeg,
    /// `lseg(point, point) -> lseg`.
    LsegConstruct,
    /// `@-@ lseg` length (`-> float8`).
    LsegLength,
    /// `@@ lseg` center / `lseg::point` (`-> point`).
    LsegCenter,
    /// `?| lseg` vertical.
    LsegVert,
    /// `?- lseg` horizontal.
    LsegHoriz,
    /// `l1 = l2` (endpoints fuzzily equal).
    LsegEq,
    /// `l1 <> l2`.
    LsegNe,
    /// `l1 < l2` (by length).
    LsegLt,
    /// `l1 <= l2` (by length).
    LsegLe,
    /// `l1 > l2` (by length).
    LsegGt,
    /// `l1 >= l2` (by length).
    LsegGe,
    /// `l1 ?|| l2` parallel.
    LsegParallel,
    /// `l1 ?-| l2` perpendicular.
    LsegPerpendicular,
    /// `l1 # l2` intersection point (`-> point`, NULL if none).
    LsegInterpt,
    /// `l1 ## l2` closest point on `l2` to `l1` (`-> point`).
    CloseSegSeg,
    /// `l1 <-> l2` segment distance (`-> float8`).
    DistSegSeg,
}

struct Signature {
    func: ScalarFn,
    args: &'static [PgType],
    ret: PgType,
}

/// A set-returning function callable in `FROM` position. Unlike [`ScalarFn`],
/// these produce a rowset (a fixed set of named output columns), so they are
/// bound as a relation rather than an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableFn {
    /// `pg_input_error_info(value text, type_name text)` — the non-throwing
    /// sibling of `pg_input_is_valid`, reporting why an input would fail.
    PgInputErrorInfo,
    /// `generate_series(start, stop [, step])` over an integer element type
    /// (`int4` or `int8`, carried here). Yields one row per value in the range.
    GenerateSeries(PgType),
    /// `jsonb_path_query(target jsonb, path jsonpath [, vars jsonb, silent bool])`
    /// — one `jsonb` row per item the path returns.
    JsonbPathQuery,
    /// `unnest(array)` over a 1-D array whose element type is carried here. Yields
    /// one row per element (NULL elements included).
    Unnest(PgType),
}

impl TableFn {
    /// The function's declared parameter types (for arity/coercion checks).
    /// `GenerateSeries` is polymorphic (int4/int8, 2- or 3-arg) and resolves via
    /// [`resolve_generate_series`] instead, so it has no fixed signature here.
    fn arg_types(self) -> &'static [PgType] {
        match self {
            TableFn::PgInputErrorInfo => &[PgType::Text, PgType::Text],
            // `GenerateSeries`/`JsonbPathQuery`/`Unnest` are polymorphic/variadic
            // and resolve their own arguments in `bind_table_fn_call`.
            TableFn::GenerateSeries(_) | TableFn::JsonbPathQuery | TableFn::Unnest(_) => &[],
        }
    }

    /// The output columns of the rowset, in order.
    pub fn columns(self) -> Vec<OutputColumn> {
        let text = |name: &str| OutputColumn::new(name, PgType::Text);
        match self {
            TableFn::PgInputErrorInfo => vec![
                text("message"),
                text("detail"),
                text("hint"),
                text("sql_error_code"),
            ],
            // A single column named after the function, of the element type.
            TableFn::GenerateSeries(elem) => vec![OutputColumn::new("generate_series", elem)],
            TableFn::JsonbPathQuery => {
                vec![OutputColumn::new("jsonb_path_query", PgType::Jsonb)]
            }
            TableFn::Unnest(elem) => vec![OutputColumn::new("unnest", elem)],
        }
    }
}

/// Resolve a set-returning function by (already lowercased) name.
pub fn lookup_table_fn(name: &str) -> Option<TableFn> {
    match name {
        "pg_input_error_info" => Some(TableFn::PgInputErrorInfo),
        _ => None,
    }
}

/// An aggregate function the executor accumulates over the rows of a group.
/// `COUNT(*)` and `COUNT(expr)` are both [`AggFn::Count`]; they differ only in
/// whether an argument expression is present (see [`crate::BoundAggregate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFn {
    Count,
    Min,
    Max,
    Sum,
    Avg,
    /// `string_agg(value text, delimiter text) -> text`: concatenates the
    /// non-NULL values of a group, separated by the (per-row) delimiter.
    StringAgg,
}

impl AggFn {
    /// The aggregate's SQL name, as it appears in error messages.
    pub fn name(self) -> &'static str {
        match self {
            AggFn::Count => "count",
            AggFn::Min => "min",
            AggFn::Max => "max",
            AggFn::Sum => "sum",
            AggFn::Avg => "avg",
            AggFn::StringAgg => "string_agg",
        }
    }
}

/// Resolve an aggregate by (already lowercased) name.
pub fn lookup_agg(name: &str) -> Option<AggFn> {
    match name {
        "count" => Some(AggFn::Count),
        "min" => Some(AggFn::Min),
        "max" => Some(AggFn::Max),
        "sum" => Some(AggFn::Sum),
        "avg" => Some(AggFn::Avg),
        "string_agg" => Some(AggFn::StringAgg),
        _ => None,
    }
}

/// The result type of an aggregate over `input_ty`, following PG 14's type
/// resolution. `input_ty` is ignored for `COUNT` (always `int8`). Returns a
/// `42883 function <name>(<type>) does not exist` error when the aggregate has
/// no overload for the argument type (e.g. `min(bit)`, `sum(text)`), matching
/// PG's report of an unresolved aggregate.
pub(crate) fn agg_return_type(
    func: AggFn,
    input_ty: PgType,
    scope: &Scope,
) -> Result<PgType, BindError> {
    let unsupported = || {
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "function {}({}) does not exist",
                func.name(),
                crate::expr::type_label(input_ty, scope.catalog().as_ref())
            ),
        )
    };
    match func {
        // COUNT is handled by the caller (arg-less for `*`); COUNT(expr) counts
        // non-null values of any type and returns bigint.
        AggFn::Count => Ok(PgType::Int8),
        // MIN/MAX return the argument type. PG defines them for most orderable
        // types, so `is_orderable` is the right starting point — but it is too
        // broad on its own: `boolean` is ordered yet has no min/max aggregate
        // (users reach for bool_and/bool_or), and neither do the text-search
        // types, which are ordered only so they can key a btree index.
        AggFn::Min | AggFn::Max => {
            let has_minmax = !matches!(input_ty, PgType::Bool | PgType::Tsvector | PgType::Tsquery);
            if has_minmax && crate::expr::is_orderable(input_ty, scope.catalog().as_ref()) {
                Ok(input_ty)
            } else {
                Err(unsupported())
            }
        }
        // SUM widens small integers to bigint and bigint to numeric to avoid
        // overflow; floats and numeric keep their type.
        AggFn::Sum => match input_ty {
            PgType::Int2 | PgType::Int4 => Ok(PgType::Int8),
            PgType::Int8 => Ok(PgType::Numeric),
            PgType::Float4 => Ok(PgType::Float4),
            PgType::Float8 => Ok(PgType::Float8),
            PgType::Numeric => Ok(PgType::Numeric),
            _ => Err(unsupported()),
        },
        // AVG of any exact type is numeric; floats average as float8.
        AggFn::Avg => match input_ty {
            PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Numeric => Ok(PgType::Numeric),
            PgType::Float4 | PgType::Float8 => Ok(PgType::Float8),
            _ => Err(unsupported()),
        },
        // `string_agg(text, text)` always returns text; `bind_aggregate` calls
        // this directly for the two-argument form's return type.
        AggFn::StringAgg => Ok(PgType::Text),
    }
}

/// Bind a table function's call arguments to typed expressions, resolving the
/// function and enforcing arity/coercion. `arg_exprs` are the raw call
/// arguments (bound in the empty scope, as SRF arguments are constants here).
pub(crate) fn bind_table_fn_call(
    name: &str,
    arg_exprs: &[ast::Expr],
    scope: &Scope,
) -> Result<(TableFn, Vec<BoundExpr>), BindError> {
    let bindings = arg_exprs
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    // `generate_series` is polymorphic on its integer element type and has two
    // arities, so it resolves outside the fixed-signature table below.
    if name == "generate_series" {
        let (elem, args) = resolve_generate_series(&bindings)?;
        return Ok((TableFn::GenerateSeries(elem), args));
    }
    if name == "jsonb_path_query" {
        return Ok((TableFn::JsonbPathQuery, resolve_jsonb_path_query(&bindings)?));
    }
    if name == "unnest" {
        let (elem, args) = resolve_unnest(&bindings)?;
        return Ok((TableFn::Unnest(elem), args));
    }
    let Some(func) = lookup_table_fn(name) else {
        return Err(undefined_function(name, &bindings));
    };
    let params = func.arg_types();
    if params.len() != bindings.len() {
        return Err(undefined_function(name, &bindings));
    }
    // Exact-type first, then a coercing pass — same policy as scalar overloads.
    for exact_only in [true, false] {
        if let Some(args) = try_coerce_args(&bindings, params, exact_only) {
            return Ok((func, args));
        }
    }
    Err(undefined_function(name, &bindings))
}

/// Resolve `unnest(array)` to its element type and single (array) argument. Only
/// the single-array 1-D form is supported; anything else is `42883`. Shared by
/// FROM-position and target-list binding.
pub(crate) fn resolve_unnest(bindings: &[Binding]) -> Result<(PgType, Vec<BoundExpr>), BindError> {
    if let [Binding::Typed(e)] = bindings
        && let PgType::Array(elem_oid) = e.ty()
        && let Some(elem) = PgType::from_oid(elem_oid)
    {
        return Ok((elem, vec![e.clone()]));
    }
    Err(undefined_function("unnest", bindings))
}

/// Resolve a `generate_series(start, stop [, step])` call to its element type
/// and coerced arguments. Supported overloads:
/// - `int4`/`int8`/`numeric` — 2 or 3 args, all of the element type;
/// - `timestamp`/`timestamptz` — 3 args `(elem, elem, interval)`.
///
/// The element type is the output column's type. Returns a `42883` "does not
/// exist" error for any other arity or argument type. Shared by FROM-position
/// and target-list binding.
pub(crate) fn resolve_generate_series(
    bindings: &[Binding],
) -> Result<(PgType, Vec<BoundExpr>), BindError> {
    let arity = bindings.len();
    if arity != 2 && arity != 3 {
        return Err(undefined_function("generate_series", bindings));
    }
    // Uniform numeric overloads: every argument (bounds and step) is the element
    // type. int4 before int8 before numeric, and exact-type before coercing — so
    // `generate_series(1, 5)` (int4 literals) stays int4, a bigint bound widens
    // to int8, and a decimal argument (typed numeric) picks numeric. Mirrors the
    // scalar overload policy in `bind_function`.
    for elem in [PgType::Int4, PgType::Int8, PgType::Numeric] {
        let params = vec![elem; arity];
        for exact_only in [true, false] {
            if let Some(args) = try_coerce_args(bindings, &params, exact_only) {
                return Ok((elem, args));
            }
        }
    }
    // Temporal overloads: 3 args `(elem, elem, interval)`, stepping a timestamp
    // by an interval. A 2-arg timestamp call matches nothing here and falls
    // through to the `42883` error, as in PG.
    if arity == 3 {
        for elem in [TS, TSTZ] {
            let params = [elem, elem, IV];
            for exact_only in [true, false] {
                if let Some(args) = try_coerce_args(bindings, &params, exact_only) {
                    return Ok((elem, args));
                }
            }
        }
    }
    Err(undefined_function("generate_series", bindings))
}

const F8: PgType = PgType::Float8;
const TS: PgType = PgType::Timestamp;
const TSTZ: PgType = PgType::TimestampTz;
const TEXT: PgType = PgType::Text;
const I4: PgType = PgType::Int4;
const IV: PgType = PgType::Interval;
const NUM: PgType = PgType::Numeric;
const DATE: PgType = PgType::Date;
const TIME: PgType = PgType::Time;
const TIMETZ: PgType = PgType::TimeTz;
const I8: PgType = PgType::Int8;
const BOOL: PgType = PgType::Bool;
const BYTEA: PgType = PgType::Bytea;
const INET: PgType = PgType::Inet;
const CIDR: PgType = PgType::Cidr;
const MONEY: PgType = PgType::Money;
const BIT: PgType = PgType::Bit;
const VARBIT: PgType = PgType::Varbit;
const MACADDR: PgType = PgType::Macaddr;
const MACADDR8: PgType = PgType::Macaddr8;
const POINT: PgType = PgType::Point;
const LSEG: PgType = PgType::Lseg;
const JSONB: PgType = PgType::Jsonb;
const JSONPATH: PgType = PgType::Jsonpath;
const TSVECTOR: PgType = PgType::Tsvector;
const TSQUERY: PgType = PgType::Tsquery;
const TEXTARR: PgType = PgType::Array(crabgresql_types::oid::TEXT);
const OID: PgType = PgType::Oid;
const REGCLASS: PgType = PgType::Reg(RegKind::Class);
const NAME: PgType = PgType::Name;

/// How many leading entries of [`SUBSTRING_SIGS`] are the regex-extraction
/// forms. `substr` is the same list without them.
const SUBSTRING_REGEX_SIGS: usize = 2;

/// `substring`'s overloads, regex forms first. `substr` is the tail of this
/// list, which is what gives the two names PG's different answers for an untyped
/// literal: `substring('abcdef', '2')` is NULL (the literal is a pattern) while
/// `substr('abcdef', '2')` is `bcdef` (it is an offset).
///
/// The order is load-bearing, not cosmetic. `coerce_for_arg` resolves a
/// non-parameter `Binding::Unknown` even under `exact_only`, so an untyped
/// literal is an exact match for *both* the text and the int4 form and the tie
/// is broken by position in `resolve_call`'s first pass. Keep the regex forms
/// leading, and keep the two names sharing one list so a new positional overload
/// cannot reach one spelling but not the other.
const SUBSTRING_SIGS: &[Signature] = &[
    Signature {
        func: ScalarFn::SubstringRegex,
        args: &[TEXT, TEXT],
        ret: TEXT,
    },
    Signature {
        func: ScalarFn::SubstringSimilar,
        args: &[TEXT, TEXT, TEXT],
        ret: TEXT,
    },
    Signature {
        func: ScalarFn::Substr,
        args: &[TEXT, I4],
        ret: TEXT,
    },
    Signature {
        func: ScalarFn::Substr,
        args: &[TEXT, I4, I4],
        ret: TEXT,
    },
    Signature {
        func: ScalarFn::SubstrBit,
        args: &[BIT, I4],
        ret: BIT,
    },
    Signature {
        func: ScalarFn::SubstrBit,
        args: &[BIT, I4, I4],
        ret: BIT,
    },
    Signature {
        func: ScalarFn::SubstrBit,
        args: &[VARBIT, I4],
        ret: VARBIT,
    },
    Signature {
        func: ScalarFn::SubstrBit,
        args: &[VARBIT, I4, I4],
        ret: VARBIT,
    },
];

/// The overloads for `name` (already lowercased). Most math functions take one
/// float8 and return float8.
fn lookup(name: &str) -> &'static [Signature] {
    macro_rules! unary_f8 {
        ($f:expr) => {
            &[Signature {
                func: $f,
                args: &[F8],
                ret: F8,
            }]
        };
    }
    // A float8 overload and a `numeric`-returns-`numeric` one. The float8 entry
    // is first so that an integer argument (which casts implicitly to either)
    // resolves to float8, as PG's preferred-type rule does; a `numeric` argument
    // still binds the numeric overload through the exact-type resolution pass.
    macro_rules! num_and_f8 {
        ($num:expr, $f8:expr) => {
            &[
                Signature {
                    func: $f8,
                    args: &[F8],
                    ret: F8,
                },
                Signature {
                    func: $num,
                    args: &[NUM],
                    ret: NUM,
                },
            ]
        };
    }
    // A `jsonb_path_*` scalar's three overloads: `(jsonb, jsonpath)` plus the
    // optional `vars jsonb` and `silent bool` arguments PG's DEFAULTs expand to.
    macro_rules! json_path_sigs {
        ($f:expr, $ret:expr) => {
            &[
                Signature { func: ScalarFn::JsonPath($f), args: &[JSONB, JSONPATH], ret: $ret },
                Signature { func: ScalarFn::JsonPath($f), args: &[JSONB, JSONPATH, JSONB], ret: $ret },
                Signature {
                    func: ScalarFn::JsonPath($f),
                    args: &[JSONB, JSONPATH, JSONB, BOOL],
                    ret: $ret,
                },
            ]
        };
    }
    // A scalar with optional trailing arguments, which PG models as one
    // overload per arity. Spelling each argument list out (rather than deriving
    // them as prefixes) keeps the positions visible, because the executor reads
    // the optional ones positionally via `args.get(N)`.
    macro_rules! arity_sigs {
        ($f:expr, $ret:expr, $($args:expr),+ $(,)?) => {
            &[$(Signature { func: $f, args: $args, ret: $ret }),+]
        };
    }
    match name {
        "trunc" => &[
            Signature {
                func: ScalarFn::Trunc,
                args: &[F8],
                ret: F8,
            },
            Signature {
                func: ScalarFn::NumTrunc,
                args: &[NUM],
                ret: NUM,
            },
            Signature {
                func: ScalarFn::NumTrunc,
                args: &[NUM, I4],
                ret: NUM,
            },
            Signature {
                func: ScalarFn::MacaddrTrunc,
                args: &[MACADDR],
                ret: MACADDR,
            },
            Signature {
                func: ScalarFn::MacaddrTrunc,
                args: &[MACADDR8],
                ret: MACADDR8,
            },
        ],
        "macaddr8_set7bit" => &[Signature {
            func: ScalarFn::Macaddr8Set7bit,
            args: &[MACADDR8],
            ret: MACADDR8,
        }],
        "round" => &[
            Signature {
                func: ScalarFn::Round,
                args: &[F8],
                ret: F8,
            },
            Signature {
                func: ScalarFn::NumRound,
                args: &[NUM],
                ret: NUM,
            },
            Signature {
                func: ScalarFn::NumRound,
                args: &[NUM, I4],
                ret: NUM,
            },
        ],
        "ceil" | "ceiling" => num_and_f8!(ScalarFn::NumCeil, ScalarFn::Ceil),
        "floor" => num_and_f8!(ScalarFn::NumFloor, ScalarFn::Floor),
        "sign" => num_and_f8!(ScalarFn::NumSign, ScalarFn::Sign),
        "sqrt" => num_and_f8!(ScalarFn::NumSqrt, ScalarFn::Sqrt),
        // numeric first: an integer argument keeps its exact value through
        // int -> numeric (PG's abs(int) is exact too); a float argument binds
        // the float8 overload.
        "abs" => &[
            Signature {
                func: ScalarFn::NumAbs,
                args: &[NUM],
                ret: NUM,
            },
            Signature {
                func: ScalarFn::AbsF8,
                args: &[F8],
                ret: F8,
            },
        ],
        // Integer overloads keep the argument type (like PG); a numeric argument
        // binds the numeric overload exactly.
        "mod" => &[
            Signature {
                func: ScalarFn::ModInt,
                args: &[PgType::Int2, PgType::Int2],
                ret: PgType::Int2,
            },
            Signature {
                func: ScalarFn::ModInt,
                args: &[I4, I4],
                ret: I4,
            },
            Signature {
                func: ScalarFn::ModInt,
                args: &[PgType::Int8, PgType::Int8],
                ret: PgType::Int8,
            },
            Signature {
                func: ScalarFn::NumMod,
                args: &[NUM, NUM],
                ret: NUM,
            },
        ],
        // money helper functions.
        "cash_words" => &[Signature {
            func: ScalarFn::CashWords,
            args: &[MONEY],
            ret: TEXT,
        }],
        "cashlarger" => &[Signature {
            func: ScalarFn::CashLarger,
            args: &[MONEY, MONEY],
            ret: MONEY,
        }],
        "cashsmaller" => &[Signature {
            func: ScalarFn::CashSmaller,
            args: &[MONEY, MONEY],
            ret: MONEY,
        }],
        "cbrt" => unary_f8!(ScalarFn::Cbrt),
        "exp" => num_and_f8!(ScalarFn::NumExp, ScalarFn::Exp),
        "ln" => num_and_f8!(ScalarFn::NumLn, ScalarFn::Ln),
        // float8 first (an integer/float argument resolves to float8, as in PG);
        // a `numeric` argument still binds the numeric overload exactly. The
        // two-arg `log(base, value)` is numeric-only.
        "log" | "log10" => &[
            Signature {
                func: ScalarFn::Log10F8,
                args: &[F8],
                ret: F8,
            },
            Signature {
                func: ScalarFn::NumLog10,
                args: &[NUM],
                ret: NUM,
            },
            Signature {
                func: ScalarFn::NumLog,
                args: &[NUM, NUM],
                ret: NUM,
            },
        ],
        "sinh" => unary_f8!(ScalarFn::Sinh),
        "cosh" => unary_f8!(ScalarFn::Cosh),
        "tanh" => unary_f8!(ScalarFn::Tanh),
        "asinh" => unary_f8!(ScalarFn::Asinh),
        "acosh" => unary_f8!(ScalarFn::Acosh),
        "atanh" => unary_f8!(ScalarFn::Atanh),
        "erf" => unary_f8!(ScalarFn::Erf),
        "erfc" => unary_f8!(ScalarFn::Erfc),
        "gamma" => unary_f8!(ScalarFn::Gamma),
        "lgamma" => unary_f8!(ScalarFn::Lgamma),
        "sind" => unary_f8!(ScalarFn::Sind),
        "cosd" => unary_f8!(ScalarFn::Cosd),
        "tand" => unary_f8!(ScalarFn::Tand),
        "cotd" => unary_f8!(ScalarFn::Cotd),
        "asind" => unary_f8!(ScalarFn::Asind),
        "acosd" => unary_f8!(ScalarFn::Acosd),
        "atand" => unary_f8!(ScalarFn::Atand),
        "power" | "pow" => &[
            Signature {
                func: ScalarFn::Power,
                args: &[F8, F8],
                ret: F8,
            },
            Signature {
                func: ScalarFn::NumPower,
                args: &[NUM, NUM],
                ret: NUM,
            },
        ],
        "atan2d" => &[Signature {
            func: ScalarFn::Atan2d,
            args: &[F8, F8],
            ret: F8,
        }],
        "float4send" => &[Signature {
            func: ScalarFn::Float4Send,
            args: &[PgType::Float4],
            ret: PgType::Bytea,
        }],
        "float8send" => &[Signature {
            func: ScalarFn::Float8Send,
            args: &[F8],
            ret: PgType::Bytea,
        }],
        "pg_input_is_valid" => &[Signature {
            func: ScalarFn::PgInputIsValid,
            args: &[PgType::Text, PgType::Text],
            ret: PgType::Bool,
        }],
        "date_part" => &[
            Signature {
                func: ScalarFn::DatePart,
                args: &[TEXT, TS],
                ret: F8,
            },
            Signature {
                func: ScalarFn::DatePartInterval,
                args: &[TEXT, IV],
                ret: F8,
            },
            Signature {
                func: ScalarFn::DatePartTz,
                args: &[TEXT, TSTZ],
                ret: F8,
            },
            Signature {
                func: ScalarFn::DatePartDate,
                args: &[TEXT, DATE],
                ret: F8,
            },
            Signature {
                func: ScalarFn::DatePartTime,
                args: &[TEXT, TIME],
                ret: F8,
            },
            Signature {
                func: ScalarFn::DatePartTimeTz,
                args: &[TEXT, TIMETZ],
                ret: F8,
            },
        ],
        "date_trunc" => &[
            Signature {
                func: ScalarFn::DateTrunc,
                args: &[TEXT, TS],
                ret: TS,
            },
            Signature {
                func: ScalarFn::DateTruncInterval,
                args: &[TEXT, IV],
                ret: IV,
            },
            Signature {
                func: ScalarFn::DateTruncTz,
                args: &[TEXT, TSTZ],
                ret: TSTZ,
            },
        ],
        "isfinite" => &[
            Signature {
                func: ScalarFn::Isfinite,
                args: &[TS],
                ret: PgType::Bool,
            },
            Signature {
                func: ScalarFn::IsfiniteInterval,
                args: &[IV],
                ret: PgType::Bool,
            },
            Signature {
                func: ScalarFn::IsfiniteTz,
                args: &[TSTZ],
                ret: PgType::Bool,
            },
            Signature {
                func: ScalarFn::IsfiniteDate,
                args: &[DATE],
                ret: PgType::Bool,
            },
        ],
        "make_date" => &[Signature {
            func: ScalarFn::MakeDate,
            args: &[I4, I4, I4],
            ret: DATE,
        }],
        "make_time" => &[Signature {
            func: ScalarFn::MakeTime,
            args: &[I4, I4, F8],
            ret: TIME,
        }],
        "make_timestamp" => &[Signature {
            func: ScalarFn::MakeTimestamp,
            args: &[I4, I4, I4, I4, I4, F8],
            ret: TS,
        }],
        "make_interval" => &[Signature {
            func: ScalarFn::MakeInterval,
            args: &[I4, I4, I4, I4, I4, I4, F8],
            ret: IV,
        }],
        "justify_days" => &[Signature {
            func: ScalarFn::JustifyDays,
            args: &[IV],
            ret: IV,
        }],
        "justify_hours" => &[Signature {
            func: ScalarFn::JustifyHours,
            args: &[IV],
            ret: IV,
        }],
        "justify_interval" => &[Signature {
            func: ScalarFn::JustifyInterval,
            args: &[IV],
            ret: IV,
        }],
        "age" => &[Signature {
            func: ScalarFn::Age,
            args: &[TS, TS],
            ret: IV,
        }],
        "to_char" => &[Signature {
            func: ScalarFn::ToCharInterval,
            args: &[IV, TEXT],
            ret: TEXT,
        }],
        "make_timestamptz" => &[
            Signature {
                func: ScalarFn::MakeTimestampTz,
                args: &[I4, I4, I4, I4, I4, F8],
                ret: TSTZ,
            },
            Signature {
                func: ScalarFn::MakeTimestampTz,
                args: &[I4, I4, I4, I4, I4, F8, TEXT],
                ret: TSTZ,
            },
        ],
        // The function form of `AT TIME ZONE`: `timezone(zone, value)`.
        "timezone" => &[
            Signature {
                func: ScalarFn::TimezoneToTz,
                args: &[TEXT, TS],
                ret: TSTZ,
            },
            Signature {
                func: ScalarFn::TimezoneToTs,
                args: &[TEXT, TSTZ],
                ret: TS,
            },
        ],
        // Two overloads. Text is listed first so a bare `md5('abc')` unknown
        // literal resolves to text; a typed `bytea` argument never coerces to
        // text (see `implicit_castable`), so `md5(x::bytea)` binds the bytea one.
        "md5" => &[
            Signature {
                func: ScalarFn::Md5,
                args: &[PgType::Text],
                ret: PgType::Text,
            },
            Signature {
                func: ScalarFn::Md5,
                args: &[PgType::Bytea],
                ret: PgType::Text,
            },
        ],
        // --- string functions ---
        // `length(bit)`/`length(varbit)` count bits, not characters. PG has only
        // `length(bit)` in the bit-string family; `char_length`/`character_length`
        // stay text-only (a bit argument there is `function does not exist`).
        "length" => &[
            Signature {
                func: ScalarFn::Length,
                args: &[TEXT],
                ret: I4,
            },
            Signature {
                func: ScalarFn::BitLen,
                args: &[BIT],
                ret: I4,
            },
            Signature {
                func: ScalarFn::BitLen,
                args: &[VARBIT],
                ret: I4,
            },
            Signature {
                func: ScalarFn::Ts(TsFn::VectorLength),
                args: &[TSVECTOR],
                ret: I4,
            },
        ],
        "char_length" | "character_length" => &[Signature {
            func: ScalarFn::Length,
            args: &[TEXT],
            ret: I4,
        }],
        // `octet_length` counts the padded bytes of a `bpchar` (via a dedicated
        // overload), while `length`/`bit_length` see the trailing-blank-trimmed
        // text value, matching PG's `bpcharoctetlen` vs `bpcharlen`/text paths.
        "octet_length" => &[
            Signature {
                func: ScalarFn::OctetLength,
                args: &[TEXT],
                ret: I4,
            },
            Signature {
                func: ScalarFn::OctetLength,
                args: &[PgType::Bpchar],
                ret: I4,
            },
        ],
        // `bit_length(bit)` is the number of bits, like `length(bit)`.
        "bit_length" => &[
            Signature {
                func: ScalarFn::BitLength,
                args: &[TEXT],
                ret: I4,
            },
            Signature {
                func: ScalarFn::BitLen,
                args: &[BIT],
                ret: I4,
            },
            Signature {
                func: ScalarFn::BitLen,
                args: &[VARBIT],
                ret: I4,
            },
        ],
        // Sequence functions. PG declares these over `regclass`; the `text`
        // overload is kept alongside it so a bare `nextval('seq')` still binds
        // without the unknown literal having to resolve through the catalog.
        // These are side-effecting and are dispatched by the executor's `eval`
        // (not `eval_scalar`).
        "nextval" => &[
            Signature {
                func: ScalarFn::Nextval,
                args: &[TEXT],
                ret: I8,
            },
            Signature {
                func: ScalarFn::Nextval,
                args: &[REGCLASS],
                ret: I8,
            },
        ],
        "currval" => &[
            Signature {
                func: ScalarFn::Currval,
                args: &[TEXT],
                ret: I8,
            },
            Signature {
                func: ScalarFn::Currval,
                args: &[REGCLASS],
                ret: I8,
            },
        ],
        "setval" => &[
            Signature {
                func: ScalarFn::Setval,
                args: &[TEXT, I8],
                ret: I8,
            },
            Signature {
                func: ScalarFn::Setval,
                args: &[REGCLASS, I8],
                ret: I8,
            },
            Signature {
                func: ScalarFn::Setval,
                args: &[TEXT, I8, BOOL],
                ret: I8,
            },
            Signature {
                func: ScalarFn::Setval,
                args: &[REGCLASS, I8, BOOL],
                ret: I8,
            },
        ],
        "lastval" => &[Signature {
            func: ScalarFn::Lastval,
            args: &[],
            ret: I8,
        }],
        // Catalog lookups. `int -> oid` is implicit, so an OID written as an
        // integer literal resolves too. Dispatched by the executor's `eval`
        // (not `eval_scalar`), which holds the session's catalog snapshot.
        "pg_get_userbyid" => &[Signature {
            func: ScalarFn::PgGetUserById,
            args: &[OID],
            ret: NAME,
        }],
        "pg_table_is_visible" => &[Signature {
            func: ScalarFn::PgTableIsVisible,
            args: &[OID],
            ret: BOOL,
        }],
        // Type formatting / node-tree deparse. Dispatched by the executor's
        // `eval` (not `eval_scalar`): `format_type` must return non-NULL for a
        // NULL type modifier, which the STRICT `eval_scalar` path cannot do.
        "format_type" => &[Signature {
            func: ScalarFn::FormatType,
            args: &[OID, I4],
            ret: TEXT,
        }],
        "pg_get_expr" => &[
            Signature {
                func: ScalarFn::PgGetExpr,
                args: &[TEXT, OID],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetExpr,
                args: &[TEXT, OID, BOOL],
                ret: TEXT,
            },
        ],
        "upper" => &[Signature {
            func: ScalarFn::Upper,
            args: &[TEXT],
            ret: TEXT,
        }],
        "lower" => &[Signature {
            func: ScalarFn::Lower,
            args: &[TEXT],
            ret: TEXT,
        }],
        "initcap" => &[Signature {
            func: ScalarFn::Initcap,
            args: &[TEXT],
            ret: TEXT,
        }],
        "substring" => SUBSTRING_SIGS,
        "substr" => &SUBSTRING_SIGS[SUBSTRING_REGEX_SIGS..],
        "strpos" => &[
            Signature {
                func: ScalarFn::StrPos,
                args: &[TEXT, TEXT],
                ret: I4,
            },
            // `POSITION(bit IN bit)` desugars to `strpos(str, sub)`.
            Signature {
                func: ScalarFn::BitPosition,
                args: &[BIT, BIT],
                ret: I4,
            },
            Signature {
                func: ScalarFn::BitPosition,
                args: &[VARBIT, VARBIT],
                ret: I4,
            },
        ],
        "overlay" => &[
            Signature {
                func: ScalarFn::Overlay,
                args: &[TEXT, TEXT, I4],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::Overlay,
                args: &[TEXT, TEXT, I4, I4],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::OverlayBit,
                args: &[BIT, BIT, I4],
                ret: BIT,
            },
            Signature {
                func: ScalarFn::OverlayBit,
                args: &[BIT, BIT, I4, I4],
                ret: BIT,
            },
        ],
        "get_bit" => &[
            Signature {
                func: ScalarFn::GetBit,
                args: &[BIT, I4],
                ret: I4,
            },
            Signature {
                func: ScalarFn::GetBit,
                args: &[VARBIT, I4],
                ret: I4,
            },
        ],
        "set_bit" => &[
            Signature {
                func: ScalarFn::SetBit,
                args: &[BIT, I4, I4],
                ret: BIT,
            },
            Signature {
                func: ScalarFn::SetBit,
                args: &[VARBIT, I4, I4],
                ret: VARBIT,
            },
        ],
        "bit_count" => &[
            Signature {
                func: ScalarFn::BitCount,
                args: &[BIT],
                ret: I8,
            },
            Signature {
                func: ScalarFn::BitCount,
                args: &[VARBIT],
                ret: I8,
            },
        ],
        "ltrim" => &[
            Signature {
                func: ScalarFn::Ltrim,
                args: &[TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::Ltrim,
                args: &[TEXT, TEXT],
                ret: TEXT,
            },
        ],
        "rtrim" => &[
            Signature {
                func: ScalarFn::Rtrim,
                args: &[TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::Rtrim,
                args: &[TEXT, TEXT],
                ret: TEXT,
            },
        ],
        "btrim" => &[
            Signature {
                func: ScalarFn::Btrim,
                args: &[TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::Btrim,
                args: &[TEXT, TEXT],
                ret: TEXT,
            },
        ],
        "lpad" => &[
            Signature {
                func: ScalarFn::Lpad,
                args: &[TEXT, I4],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::Lpad,
                args: &[TEXT, I4, TEXT],
                ret: TEXT,
            },
        ],
        "rpad" => &[
            Signature {
                func: ScalarFn::Rpad,
                args: &[TEXT, I4],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::Rpad,
                args: &[TEXT, I4, TEXT],
                ret: TEXT,
            },
        ],
        "replace" => &[Signature {
            func: ScalarFn::Replace,
            args: &[TEXT, TEXT, TEXT],
            ret: TEXT,
        }],
        // The `regexp_*` family, whose trailing `start`/`n`/`flags`/`subexpr`
        // arguments are all optional. `regexp_replace` has two 4-argument forms
        // that differ only in the last argument's type; the flags form is
        // listed first because that is what an untyped literal resolves to (an
        // integer literal binds as `int4` and picks the other by exact match).
        "regexp_replace" => arity_sigs!(
            ScalarFn::RegexpReplace,
            TEXT,
            &[TEXT, TEXT, TEXT],
            &[TEXT, TEXT, TEXT, TEXT],
            &[TEXT, TEXT, TEXT, I4],
            &[TEXT, TEXT, TEXT, I4, I4],
            &[TEXT, TEXT, TEXT, I4, I4, TEXT],
        ),
        "regexp_like" => arity_sigs!(
            ScalarFn::RegexpLike,
            BOOL,
            &[TEXT, TEXT],
            &[TEXT, TEXT, TEXT],
        ),
        "regexp_count" => arity_sigs!(
            ScalarFn::RegexpCount,
            I4,
            &[TEXT, TEXT],
            &[TEXT, TEXT, I4],
            &[TEXT, TEXT, I4, TEXT],
        ),
        "regexp_substr" => arity_sigs!(
            ScalarFn::RegexpSubstr,
            TEXT,
            &[TEXT, TEXT],
            &[TEXT, TEXT, I4],
            &[TEXT, TEXT, I4, I4],
            &[TEXT, TEXT, I4, I4, TEXT],
            &[TEXT, TEXT, I4, I4, TEXT, I4],
        ),
        "translate" => &[Signature {
            func: ScalarFn::Translate,
            args: &[TEXT, TEXT, TEXT],
            ret: TEXT,
        }],
        "repeat" => &[Signature {
            func: ScalarFn::Repeat,
            args: &[TEXT, I4],
            ret: TEXT,
        }],
        "reverse" => &[Signature {
            func: ScalarFn::Reverse,
            args: &[TEXT],
            ret: TEXT,
        }],
        "left" => &[Signature {
            func: ScalarFn::Left,
            args: &[TEXT, I4],
            ret: TEXT,
        }],
        "right" => &[Signature {
            func: ScalarFn::Right,
            args: &[TEXT, I4],
            ret: TEXT,
        }],
        "ascii" => &[Signature {
            func: ScalarFn::Ascii,
            args: &[TEXT],
            ret: I4,
        }],
        "chr" => &[Signature {
            func: ScalarFn::Chr,
            args: &[I4],
            ret: TEXT,
        }],
        "split_part" => &[Signature {
            func: ScalarFn::SplitPart,
            args: &[TEXT, TEXT, I4],
            ret: TEXT,
        }],
        "starts_with" => &[Signature {
            func: ScalarFn::StartsWith,
            args: &[TEXT, TEXT],
            ret: BOOL,
        }],
        "to_hex" => &[
            Signature {
                func: ScalarFn::ToHex,
                args: &[I4],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToHexInt8,
                args: &[I8],
                ret: TEXT,
            },
        ],
        "encode" => &[Signature {
            func: ScalarFn::Encode,
            args: &[BYTEA, TEXT],
            ret: TEXT,
        }],
        "decode" => &[Signature {
            func: ScalarFn::Decode,
            args: &[TEXT, TEXT],
            ret: BYTEA,
        }],
        "quote_ident" => &[Signature {
            func: ScalarFn::QuoteIdent,
            args: &[TEXT],
            ret: TEXT,
        }],
        "quote_literal" => &[Signature {
            func: ScalarFn::QuoteLiteral,
            args: &[TEXT],
            ret: TEXT,
        }],
        "quote_nullable" => &[Signature {
            func: ScalarFn::QuoteNullable,
            args: &[TEXT],
            ret: TEXT,
        }],
        // inet/cidr accessors. A `cidr` argument coerces to the `inet` overload
        // via the implicit cidr->inet cast, matching PG (whose inet functions
        // accept cidr). `abbrev` keeps a distinct cidr overload because its
        // output differs (`10.1/16` vs `10.1.0.0/16`); the inet overload is
        // listed first so an untyped literal resolves to inet (PG's preferred
        // type in the inet/cidr category), while a typed cidr still binds cidr.
        "host" => &[Signature {
            func: ScalarFn::Host,
            args: &[INET],
            ret: TEXT,
        }],
        "masklen" => &[Signature {
            func: ScalarFn::Masklen,
            args: &[INET],
            ret: I4,
        }],
        "family" => &[Signature {
            func: ScalarFn::Family,
            args: &[INET],
            ret: I4,
        }],
        "network" => &[Signature {
            func: ScalarFn::Network,
            args: &[INET],
            ret: CIDR,
        }],
        "abbrev" => &[
            Signature {
                func: ScalarFn::AbbrevInet,
                args: &[INET],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::AbbrevCidr,
                args: &[CIDR],
                ret: TEXT,
            },
        ],
        // --- geometric constructors / accessors ---
        "point" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PointConstruct),
            args: &[F8, F8],
            ret: POINT,
        }],
        "lseg" => &[Signature {
            func: ScalarFn::Geo(GeoFn::LsegConstruct),
            args: &[POINT, POINT],
            ret: LSEG,
        }],
        "slope" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PointSlope),
            args: &[POINT, POINT],
            ret: F8,
        }],
        "ishorizontal" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PointHoriz),
            args: &[POINT, POINT],
            ret: BOOL,
        }],
        "isvertical" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PointVert),
            args: &[POINT, POINT],
            ret: BOOL,
        }],
        // --- jsonpath query functions: each has the 2-arg form plus the
        // optional `vars jsonb` / `silent bool` arguments PG's DEFAULTs add ---
        // --- text search (tsvector / tsquery) ---
        "strip" => &[Signature {
            func: ScalarFn::Ts(TsFn::Strip),
            args: &[TSVECTOR],
            ret: TSVECTOR,
        }],
        "setweight" => &[
            Signature {
                func: ScalarFn::Ts(TsFn::SetWeight),
                args: &[TSVECTOR, TEXT],
                ret: TSVECTOR,
            },
            Signature {
                func: ScalarFn::Ts(TsFn::SetWeightLexemes),
                args: &[TSVECTOR, TEXT, TEXTARR],
                ret: TSVECTOR,
            },
        ],
        // Both `ts_delete` overloads share one variant; the executor branches on
        // whether the second argument arrived as text or as an array.
        "ts_delete" => &[
            Signature {
                func: ScalarFn::Ts(TsFn::Delete),
                args: &[TSVECTOR, TEXT],
                ret: TSVECTOR,
            },
            Signature {
                func: ScalarFn::Ts(TsFn::Delete),
                args: &[TSVECTOR, TEXTARR],
                ret: TSVECTOR,
            },
        ],
        "ts_filter" => &[Signature {
            func: ScalarFn::Ts(TsFn::Filter),
            args: &[TSVECTOR, TEXTARR],
            ret: TSVECTOR,
        }],
        "tsvector_to_array" => &[Signature {
            func: ScalarFn::Ts(TsFn::VectorToArray),
            args: &[TSVECTOR],
            ret: TEXTARR,
        }],
        "array_to_tsvector" => &[Signature {
            func: ScalarFn::Ts(TsFn::ArrayToVector),
            args: &[TEXTARR],
            ret: TSVECTOR,
        }],
        "numnode" => &[Signature {
            func: ScalarFn::Ts(TsFn::NumNode),
            args: &[TSQUERY],
            ret: I4,
        }],
        "querytree" => &[Signature {
            func: ScalarFn::Ts(TsFn::QueryTree),
            args: &[TSQUERY],
            ret: TEXT,
        }],
        "tsquery_phrase" => &[
            Signature {
                func: ScalarFn::Ts(TsFn::QueryPhrase),
                args: &[TSQUERY, TSQUERY],
                ret: TSQUERY,
            },
            Signature {
                func: ScalarFn::Ts(TsFn::QueryPhraseDist),
                args: &[TSQUERY, TSQUERY, I4],
                ret: TSQUERY,
            },
        ],
        "jsonb_path_exists" => json_path_sigs!(JsonPathFn::Exists, BOOL),
        "jsonb_path_match" => json_path_sigs!(JsonPathFn::Match, BOOL),
        "jsonb_path_query_array" => json_path_sigs!(JsonPathFn::QueryArray, JSONB),
        "jsonb_path_query_first" => json_path_sigs!(JsonPathFn::QueryFirst, JSONB),
        _ => &[],
    }
}

/// The last part of a function name, lowercased (`pg_catalog.abs` → `abs`).
fn function_name(name: &ast::ObjectName) -> Option<String> {
    name.0
        .last()
        .and_then(|p| p.as_ident())
        .map(crate::expr::normalize_ident)
}

pub(crate) fn bind_function(func: &ast::Function, scope: &Scope) -> Result<Binding, BindError> {
    if func.over.is_some()
        || func.filter.is_some()
        || !func.within_group.is_empty()
        || func.null_treatment.is_some()
    {
        return Err(BindError::feature_not_supported(
            "this function form is not supported yet",
        ));
    }
    let Some(name) = function_name(&func.name) else {
        return Err(BindError::feature_not_supported(format!(
            "function is not supported yet: {func}"
        )));
    };
    // Aggregates bind to a transient `Aggregate` marker (extracted into an
    // `Aggregate` plan node later), not to a scalar overload.
    if let Some(agg) = lookup_agg(&name) {
        return bind_aggregate(agg, &name, &func.args, scope);
    }
    let arg_exprs = positional_args(&func.args)?;
    let bindings = arg_exprs
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;

    // `concat`/`concat_ws`/`format` are variadic and non-strict; they don't fit
    // the fixed-arity overload table, so every argument is coerced to text and a
    // single variadic `FuncCall` is built directly.
    if let Some(func) = match name.as_str() {
        "concat" => Some(ScalarFn::Concat),
        "concat_ws" => Some(ScalarFn::ConcatWs),
        "format" => Some(ScalarFn::Format),
        _ => None,
    } {
        let args = bindings
            .into_iter()
            .map(crate::expr::to_concat_operand)
            .collect::<Result<Vec<_>, _>>()?;
        return finish_func_call(func, PgType::Text, args);
    }

    // Polymorphic array functions can't live in the fixed-signature overload
    // table (their argument/result types depend on the array's element type).
    if matches!(
        name.as_str(),
        "cardinality"
            | "array_length"
            | "array_upper"
            | "array_append"
            | "array_prepend"
            | "array_cat"
            | "array_to_string"
    ) && let Some(binding) = crate::expr::bind_array_function(&name, &bindings)?
    {
        return Ok(binding);
    }

    resolve_call(&name, bindings, scope.catalog())
}

/// Bind an aggregate call (`count(*)`, `min(x)`, `sum(a + b)`, …) to a transient
/// [`BoundExpr::Aggregate`] marker. The binder's extraction pass later moves it
/// into a [`crate::LogicalPlan::Aggregate`] node and replaces the marker with a
/// `ColumnRef`. `FILTER`/`OVER`/`WITHIN GROUP` were already rejected by the
/// caller; per-aggregate `ORDER BY` is rejected here.
fn bind_aggregate(
    agg: AggFn,
    name: &str,
    args: &ast::FunctionArguments,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let list = match args {
        ast::FunctionArguments::List(list) => list,
        // `count` with no parentheses (`SELECT count;`) is a column reference,
        // not a call, and never reaches here; any other parenless form is an
        // unknown aggregate signature.
        ast::FunctionArguments::None => {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!("function {name}() does not exist"),
            ));
        }
        ast::FunctionArguments::Subquery(_) => {
            return Err(BindError::feature_not_supported(
                "subquery function arguments are not supported yet",
            ));
        }
    };
    let distinct = matches!(
        list.duplicate_treatment,
        Some(ast::DuplicateTreatment::Distinct)
    );
    if !list.clauses.is_empty() {
        return Err(BindError::feature_not_supported(
            "aggregate ORDER BY / WITHIN GROUP is not supported yet",
        ));
    }

    let has_wildcard = list.args.iter().any(|a| {
        matches!(
            a,
            ast::FunctionArg::Unnamed(
                ast::FunctionArgExpr::Wildcard | ast::FunctionArgExpr::QualifiedWildcard(_)
            )
        )
    });
    // `ALL`/`DISTINCT` only apply to expression arguments; PostgreSQL rejects
    // `count(DISTINCT *)` during parsing. Our parser retains this form, so keep
    // the observable syntax error at bind time.
    if list.duplicate_treatment.is_some() && has_wildcard {
        return Err(BindError::syntax("syntax error at or near \"*\""));
    }

    // `count(*)` — the only aggregate taking the row-wildcard argument. It counts
    // every row (no NULL skipping), so it carries no argument expression.
    if agg == AggFn::Count
        && matches!(
            list.args.as_slice(),
            [ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Wildcard)]
        )
    {
        return Ok(Binding::Typed(BoundExpr::Aggregate {
            func: AggFn::Count,
            distinct,
            args: Vec::new(),
            input_ty: PgType::Int8,
            ret: PgType::Int8,
        }));
    }

    // A row-wildcard argument to any other aggregate (`sum(*)`) has no overload;
    // PG reports it like a zero-argument call.
    if has_wildcard {
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!("function {name}() does not exist"),
        ));
    }
    // A parameterless call: `count()` has PG's dedicated hint; the rest are just
    // an unresolved zero-argument overload.
    if list.args.is_empty() {
        return Err(if agg == AggFn::Count {
            BindError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                "count(*) must be used to call a parameterless aggregate function",
            )
        } else {
            BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!("function {name}() does not exist"),
            )
        });
    }

    // Bind each argument (an unknown literal resolves to text, as in a bare
    // projection) so a wrong-arity error can name the actual argument types, as
    // PG does.
    let arg_exprs = positional_arg_exprs(&list.args)?;
    let mut bound = arg_exprs
        .iter()
        .map(|e| crate::expr::bind_scalar(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    let undefined_arity = || {
        let types = bound
            .iter()
            .map(|b| b.ty().name())
            .collect::<Vec<_>>()
            .join(", ");
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!("function {name}({types}) does not exist"),
        )
    };

    // `string_agg(value text, delimiter text)` is the only two-argument
    // aggregate. Both arguments must be text (PG defines only the text and bytea
    // overloads); a non-text-family argument has no overload.
    if agg == AggFn::StringAgg {
        if bound.len() != 2 || !bound.iter().all(|b| crate::expr::is_text_family(b.ty())) {
            return Err(undefined_arity());
        }
        if distinct {
            return Err(BindError::feature_not_supported(
                "string_agg(DISTINCT ...) is not supported yet",
            ));
        }
        let delim = crate::expr::coerce_expr(bound.pop().expect("delimiter"), PgType::Text)?;
        let value = crate::expr::coerce_expr(bound.pop().expect("value"), PgType::Text)?;
        return Ok(Binding::Typed(BoundExpr::Aggregate {
            func: AggFn::StringAgg,
            distinct: false,
            args: vec![value, delim],
            input_ty: PgType::Text,
            ret: agg_return_type(agg, PgType::Text, scope)?,
        }));
    }

    // Every other supported aggregate is unary.
    if bound.len() != 1 {
        return Err(undefined_arity());
    }
    let arg = bound.pop().expect("exactly one argument");
    let input_ty = arg.ty();
    // A DISTINCT aggregate must compare its inputs for equality; a type with no
    // usable equality (e.g. `point`/`lseg`, which are not orderable) reports
    // PG's error rather than reaching the executor's comparison and panicking.
    if distinct && !crate::expr::is_orderable(input_ty, scope.catalog().as_ref()) {
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "could not identify an equality operator for type {}",
                crate::expr::type_label(input_ty, scope.catalog().as_ref())
            ),
        ));
    }
    let ret = agg_return_type(agg, input_ty, scope)?;
    Ok(Binding::Typed(BoundExpr::Aggregate {
        func: agg,
        distinct,
        args: vec![arg],
        input_ty,
        ret,
    }))
}

/// Resolve an overload for `name` given already-bound arguments, then build the
/// `FuncCall` node. Shared by ordinary function calls and the `CEIL`/`FLOOR`
/// special-syntax expressions.
/// Build a `FuncCall` binding, rejecting conflicting explicit `COLLATE`
/// clauses among `args` first (`concat('a' COLLATE x, 'b' COLLATE y)` is
/// `42P22` the same way `a COLLATE x = b COLLATE y` is). Shared by every
/// `FuncCall` construction site so the check isn't duplicated at each one.
fn finish_func_call(func: ScalarFn, ret: PgType, args: Vec<BoundExpr>) -> Result<Binding, BindError> {
    if ret.is_collatable() || args.iter().any(|a| a.ty().is_collatable()) {
        crate::collation::check_explicit_conflict(
            args.iter().map(crate::collation::expr_collation),
        )?;
    }
    Ok(Binding::Typed(BoundExpr::FuncCall { func, ret, args }))
}

pub(crate) fn resolve_call(
    name: &str,
    bindings: Vec<Binding>,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<Binding, BindError> {
    let sigs = lookup(name);
    if sigs.is_empty() {
        // No built-in of this name: a user-defined `LANGUAGE SQL` function may
        // still match. Only when that also fails is the call undefined.
        return resolve_user_routine_call(name, bindings, catalog);
    }
    // First try an all-exact-type match. Then, among the signatures whose args
    // all coerce, pick the one keeping the most arguments at their exact type —
    // so `power(numeric, int)` prefers `power(numeric, numeric)` over the float8
    // overload, as PG's preferred-type resolution does. Ties keep list order
    // (float8 listed first, the preferred numeric-category type).
    for sig in sigs {
        if sig.args.len() == bindings.len()
            && let Some(args) = try_coerce_args(&bindings, sig.args, true)
        {
            return finish_func_call(sig.func, sig.ret, args);
        }
    }
    let mut best: Option<(usize, &Signature, Vec<BoundExpr>)> = None;
    for sig in sigs {
        if sig.args.len() != bindings.len() {
            continue;
        }
        if let Some(args) = try_coerce_args(&bindings, sig.args, false) {
            let exact = bindings
                .iter()
                .zip(sig.args)
                .filter(|(b, target)| matches!(b, Binding::Typed(e) if e.ty() == **target))
                .count();
            if best.as_ref().is_none_or(|(b, _, _)| exact > *b) {
                best = Some((exact, sig, args));
            }
        }
    }
    match best {
        Some((_, sig, args)) => finish_func_call(sig.func, sig.ret, args),
        // No built-in overload fit the argument types; a user `LANGUAGE SQL`
        // function of the same name but different signature may still match.
        None => resolve_user_routine_call(name, bindings, catalog),
    }
}

/// Maximum depth of nested `LANGUAGE SQL` inlining. A validated function cannot
/// reference itself (it is registered only after its body binds), so this only
/// guards against a pathological chain and never trips for legitimate calls.
const MAX_INLINE_DEPTH: u32 = 100;

thread_local! {
    /// Current SQL-function inlining depth on this (single-threaded) bind. A RAII
    /// guard keeps it balanced across the `?` early-returns inside inlining.
    static INLINE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

struct InlineGuard;

impl InlineGuard {
    /// Enter one inlining level, or error if the nesting limit is reached.
    fn enter() -> Result<InlineGuard, BindError> {
        let depth = INLINE_DEPTH.with(|d| {
            let next = d.get() + 1;
            d.set(next);
            next
        });
        if depth > MAX_INLINE_DEPTH {
            INLINE_DEPTH.with(|d| d.set(d.get() - 1));
            // PG's ERRCODE_STATEMENT_TOO_COMPLEX ("stack depth limit exceeded").
            return Err(BindError::new(
                "54001",
                "SQL function inlining nested too deeply",
            ));
        }
        Ok(InlineGuard)
    }
}

impl Drop for InlineGuard {
    fn drop(&mut self) {
        INLINE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Resolve a call that matched no built-in against the user-defined routines of
/// that name.
///
/// A `LANGUAGE SQL` body is expanded inline, its `$n` leaves replaced by the
/// (coerced) call arguments; anything else becomes a [`BoundExpr::Routine`] the
/// executor dispatches at run time. Returns PG's `42883` "function name(types)
/// does not exist" when nothing matches, or `42725` "function name(types) is not
/// unique" when two overloads coerce equally well.
fn resolve_user_routine_call(
    name: &str,
    bindings: Vec<Binding>,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<Binding, BindError> {
    let sigs = catalog.routines(name);
    let Some((sig, args)) = choose_routine_overload(name, &bindings, &sigs)? else {
        return Err(undefined_function(name, &bindings));
    };

    // A procedure is not callable as a function, even when its signature is the
    // only match — PG reports 42809 with the CALL hint rather than 42883.
    if sig.kind == RoutineKind::Procedure {
        let arglist: Vec<&str> = sig.arg_types.iter().map(|t| t.name()).collect();
        return Err(BindError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("{name}({}) is a procedure", arglist.join(", ")),
        )
        .with_hint(Some("To call a procedure, use CALL.".into())));
    }

    let body = match &sig.imp {
        RoutineImpl::Sql(body) => body,
        // Not inlinable: the body is an imperative program, so the call
        // survives to execution as a marker carrying the routine's OID.
        RoutineImpl::PlPgSql => {
            return Ok(Binding::Typed(BoundExpr::Routine {
                oid: sig.oid,
                name: Arc::from(sig.name.as_str()),
                arg_types: Arc::from(sig.arg_types.as_slice()),
                strict: sig.strict,
                args,
                ret: sig.return_type,
            }));
        }
    };

    let _guard = InlineGuard::enter()?;
    let body = bind_sql_function_body(
        catalog,
        &sig.name,
        &sig.arg_types,
        &sig.arg_names,
        sig.return_type,
        body,
    )?;
    // Inlining substitutes each argument into every `$n` occurrence, so a volatile
    // argument (e.g. `nextval`) referenced more than once would run its side
    // effect / re-roll its value once per occurrence — diverging from PG, which
    // evaluates each argument once. Refuse that inline rather than mis-evaluate.
    for (i, arg) in args.iter().enumerate() {
        if arg.contains_volatile_fn() && body.count_param_refs(i) >= 2 {
            return Err(BindError::feature_not_supported(
                "a volatile function argument referenced more than once in a \
                 SQL function body is not supported yet",
            ));
        }
    }
    Ok(Binding::Typed(inline_params(body, &args)))
}

/// Pick the winning user-routine overload for a call, mirroring the built-in
/// resolver's preference: an all-exact-type match wins outright; otherwise the
/// argument-coercible candidate keeping the most arguments at their exact type
/// wins, and a tie at that best score is PG's `42725` ambiguity. Returns
/// `Ok(None)` when no overload's arity/types fit (an undefined function).
fn choose_routine_overload<'a>(
    name: &str,
    bindings: &[Binding],
    sigs: &'a [RoutineSig],
) -> Result<Option<(&'a RoutineSig, Vec<BoundExpr>)>, BindError> {
    // Two overloads can never share the same argument types (`create_function`
    // rejects that), so at most one all-exact match exists.
    for sig in sigs {
        if sig.arg_types.len() == bindings.len()
            && let Some(args) = try_coerce_args(bindings, &sig.arg_types, true)
        {
            return Ok(Some((sig, args)));
        }
    }
    // No exact match: rank the coercible candidates by how many arguments are
    // already at their exact type, as the built-in resolver does.
    let mut best: Option<(usize, &RoutineSig, Vec<BoundExpr>)> = None;
    let mut tied = false;
    for sig in sigs {
        if sig.arg_types.len() != bindings.len() {
            continue;
        }
        let Some(args) = try_coerce_args(bindings, &sig.arg_types, false) else {
            continue;
        };
        let score = bindings
            .iter()
            .zip(&sig.arg_types)
            .filter(|(b, target)| matches!(b, Binding::Typed(e) if e.ty() == **target))
            .count();
        match &best {
            None => best = Some((score, sig, args)),
            Some((b, _, _)) if score > *b => {
                best = Some((score, sig, args));
                tied = false;
            }
            Some((b, _, _)) if score == *b => tied = true,
            _ => {}
        }
    }
    if tied {
        return Err(ambiguous_function(name, bindings));
    }
    Ok(best.map(|(_, sig, args)| (sig, args)))
}

/// Bind a `CEIL(x)` / `FLOOR(x)` expression (sqlparser parses these as dedicated
/// AST nodes rather than function calls). The `TO`/scale forms are not
/// supported; the plain form resolves the same overloads as `ceil(...)`.
pub(crate) fn bind_ceil_floor(
    name: &str,
    expr: &ast::Expr,
    field: &ast::CeilFloorKind,
    scope: &Scope,
) -> Result<Binding, BindError> {
    if !matches!(
        field,
        ast::CeilFloorKind::DateTimeField(ast::DateTimeField::NoDateTime)
    ) {
        return Err(BindError::feature_not_supported(format!(
            "{name}(... TO ...) is not supported yet"
        )));
    }
    let arg = bind_expr(expr, scope)?;
    resolve_call(name, vec![arg], scope.catalog())
}

/// If `func` is a top-level call to a set-returning function usable in the
/// SELECT target list (currently `generate_series`), bind it to a
/// [`BoundExpr::Srf`] marker. Returns `Ok(None)` when it is not such a call, so
/// the caller can bind it as an ordinary scalar instead.
pub(crate) fn bind_srf_projection(
    func: &ast::Function,
    scope: &Scope,
) -> Result<Option<BoundExpr>, BindError> {
    // Only plain positional calls can be set-returning here; window/filter/etc.
    // forms are never set-returning in a target list.
    if func.over.is_some()
        || func.filter.is_some()
        || !func.within_group.is_empty()
        || func.null_treatment.is_some()
    {
        return Ok(None);
    }
    let Some(name) = function_name(&func.name) else {
        return Ok(None);
    };
    if name != "generate_series" && name != "jsonb_path_query" && name != "unnest" {
        return Ok(None);
    }
    let arg_exprs = positional_args(&func.args)?;
    let bindings = arg_exprs
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    if name == "jsonb_path_query" {
        let args = resolve_jsonb_path_query(&bindings)?;
        return Ok(Some(BoundExpr::Srf {
            func: TableFn::JsonbPathQuery,
            ret: PgType::Jsonb,
            args,
        }));
    }
    if name == "unnest" {
        let (elem, args) = resolve_unnest(&bindings)?;
        return Ok(Some(BoundExpr::Srf {
            func: TableFn::Unnest(elem),
            ret: elem,
            args,
        }));
    }
    let (elem, args) = resolve_generate_series(&bindings)?;
    Ok(Some(BoundExpr::Srf {
        func: TableFn::GenerateSeries(elem),
        ret: elem,
        args,
    }))
}

/// Resolve a `jsonb_path_query(target, path [, vars, silent])` call to its
/// coerced argument list (`jsonb`, `jsonpath`, and the optional `vars jsonb` /
/// `silent bool`). Shared by FROM-position and target-list binding.
pub(crate) fn resolve_jsonb_path_query(bindings: &[Binding]) -> Result<Vec<BoundExpr>, BindError> {
    let params: &[PgType] = match bindings.len() {
        2 => &[JSONB, JSONPATH],
        3 => &[JSONB, JSONPATH, JSONB],
        4 => &[JSONB, JSONPATH, JSONB, BOOL],
        _ => return Err(undefined_function("jsonb_path_query", bindings)),
    };
    for exact_only in [true, false] {
        if let Some(args) = try_coerce_args(bindings, params, exact_only) {
            return Ok(args);
        }
    }
    Err(undefined_function("jsonb_path_query", bindings))
}

/// Try to coerce every binding to the signature's parameter types. When
/// `exact_only`, reject anything that would need a numeric promotion.
fn try_coerce_args(
    bindings: &[Binding],
    params: &[PgType],
    exact_only: bool,
) -> Option<Vec<BoundExpr>> {
    let mut out = Vec::with_capacity(params.len());
    for (binding, &target) in bindings.iter().zip(params) {
        out.push(coerce_for_arg(binding.clone(), target, exact_only)?);
    }
    Some(out)
}

fn undefined_function(name: &str, bindings: &[Binding]) -> BindError {
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!(
            "function {name}({}) does not exist",
            call_type_list(bindings)
        ),
    )
}

/// PG's `42725` for a call that matches two overloads equally well, with the same
/// DETAIL/HINT PostgreSQL prints (see [`crate::expr`]'s operator-ambiguity error).
fn ambiguous_function(name: &str, bindings: &[Binding]) -> BindError {
    BindError::new(
        sqlstate::AMBIGUOUS_FUNCTION,
        format!(
            "function {name}({}) is not unique",
            call_type_list(bindings)
        ),
    )
    .with_detail(Some(
        "Could not choose a best candidate function.".to_string(),
    ))
    .with_hint(Some(
        "You might need to add explicit type casts.".to_string(),
    ))
}

/// The comma-separated argument type list rendered for a function-resolution
/// error message (`integer, text`), matching PG's `func(types)` spelling.
fn call_type_list(bindings: &[Binding]) -> String {
    bindings
        .iter()
        .map(crate::expr::binding_type_label)
        .collect::<Vec<_>>()
        .join(", ")
}

fn positional_args(args: &ast::FunctionArguments) -> Result<Vec<ast::Expr>, BindError> {
    let list = match args {
        ast::FunctionArguments::None => return Ok(Vec::new()),
        ast::FunctionArguments::List(list) => list,
        ast::FunctionArguments::Subquery(_) => {
            return Err(BindError::feature_not_supported(
                "subquery function arguments are not supported yet",
            ));
        }
    };
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Err(BindError::feature_not_supported(
            "this function argument form is not supported yet",
        ));
    }
    positional_arg_exprs(&list.args)
}

/// Extract plain positional argument expressions, rejecting named/wildcard
/// forms. Shared by scalar calls and table-function (FROM-position) calls.
pub(crate) fn positional_arg_exprs(args: &[ast::FunctionArg]) -> Result<Vec<ast::Expr>, BindError> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => out.push(e.clone()),
            _ => {
                return Err(BindError::feature_not_supported(
                    "named or wildcard function arguments are not supported yet",
                ));
            }
        }
    }
    Ok(out)
}
