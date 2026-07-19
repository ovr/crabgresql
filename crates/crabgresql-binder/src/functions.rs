//! Scalar function resolution.
//!
//! Clean-room (see AGENTS.md): the function set, argument coercions, and error
//! text reproduce PG's *observable* behavior for the functions the float
//! regression tests call, pinned by the corpus. A minimal name+arity+coercion
//! resolver stands in for PG's full overload machinery — enough for these
//! tests, where arguments are floats, unknown literals, or ints promoted to
//! float8.

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::PgType;

use crate::expr::{Binding, BoundExpr, Scope, bind_expr, coerce_for_arg};
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
}

impl TableFn {
    /// The function's declared parameter types (for arity/coercion checks).
    /// `GenerateSeries` is polymorphic (int4/int8, 2- or 3-arg) and resolves via
    /// [`resolve_generate_series`] instead, so it has no fixed signature here.
    fn arg_types(self) -> &'static [PgType] {
        match self {
            TableFn::PgInputErrorInfo => &[PgType::Text, PgType::Text],
            TableFn::GenerateSeries(_) => &[],
        }
    }

    /// The output columns of the rowset, in order.
    pub fn columns(self) -> Vec<OutputColumn> {
        let text = |name: &str| OutputColumn {
            name: name.to_string(),
            ty: PgType::Text,
        };
        match self {
            TableFn::PgInputErrorInfo => vec![
                text("message"),
                text("detail"),
                text("hint"),
                text("sql_error_code"),
            ],
            // A single column named after the function, of the element type.
            TableFn::GenerateSeries(elem) => vec![OutputColumn {
                name: "generate_series".to_string(),
                ty: elem,
            }],
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
        _ => None,
    }
}

/// The result type of an aggregate over `input_ty`, following PG 14's type
/// resolution. `input_ty` is ignored for `COUNT` (always `int8`). Returns a
/// `42883 function <name>(<type>) does not exist` error when the aggregate has
/// no overload for the argument type (e.g. `min(bit)`, `sum(text)`), matching
/// PG's report of an unresolved aggregate.
pub(crate) fn agg_return_type(func: AggFn, input_ty: PgType) -> Result<PgType, BindError> {
    let unsupported = || {
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "function {}({}) does not exist",
                func.name(),
                input_ty.name()
            ),
        )
    };
    match func {
        // COUNT is handled by the caller (arg-less for `*`); COUNT(expr) counts
        // non-null values of any type and returns bigint.
        AggFn::Count => Ok(PgType::Int8),
        // MIN/MAX return the argument type. PG defines them for every orderable
        // type *except* boolean (users reach for bool_and/bool_or there), so
        // `is_orderable` — which includes bool for ORDER BY — is too broad here.
        AggFn::Min | AggFn::Max => {
            if input_ty != PgType::Bool && crate::expr::is_orderable(input_ty) {
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
                Signature { func: $f8, args: &[F8], ret: F8 },
                Signature { func: $num, args: &[NUM], ret: NUM },
            ]
        };
    }
    match name {
        "trunc" => &[
            Signature { func: ScalarFn::Trunc, args: &[F8], ret: F8 },
            Signature { func: ScalarFn::NumTrunc, args: &[NUM], ret: NUM },
            Signature { func: ScalarFn::NumTrunc, args: &[NUM, I4], ret: NUM },
            Signature { func: ScalarFn::MacaddrTrunc, args: &[MACADDR], ret: MACADDR },
            Signature { func: ScalarFn::MacaddrTrunc, args: &[MACADDR8], ret: MACADDR8 },
        ],
        "macaddr8_set7bit" => &[Signature {
            func: ScalarFn::Macaddr8Set7bit,
            args: &[MACADDR8],
            ret: MACADDR8,
        }],
        "round" => &[
            Signature { func: ScalarFn::Round, args: &[F8], ret: F8 },
            Signature { func: ScalarFn::NumRound, args: &[NUM], ret: NUM },
            Signature { func: ScalarFn::NumRound, args: &[NUM, I4], ret: NUM },
        ],
        "ceil" | "ceiling" => num_and_f8!(ScalarFn::NumCeil, ScalarFn::Ceil),
        "floor" => num_and_f8!(ScalarFn::NumFloor, ScalarFn::Floor),
        "sign" => num_and_f8!(ScalarFn::NumSign, ScalarFn::Sign),
        "sqrt" => num_and_f8!(ScalarFn::NumSqrt, ScalarFn::Sqrt),
        // numeric first: an integer argument keeps its exact value through
        // int -> numeric (PG's abs(int) is exact too); a float argument binds
        // the float8 overload.
        "abs" => &[
            Signature { func: ScalarFn::NumAbs, args: &[NUM], ret: NUM },
            Signature { func: ScalarFn::AbsF8, args: &[F8], ret: F8 },
        ],
        // Integer overloads keep the argument type (like PG); a numeric argument
        // binds the numeric overload exactly.
        "mod" => &[
            Signature { func: ScalarFn::ModInt, args: &[PgType::Int2, PgType::Int2], ret: PgType::Int2 },
            Signature { func: ScalarFn::ModInt, args: &[I4, I4], ret: I4 },
            Signature { func: ScalarFn::ModInt, args: &[PgType::Int8, PgType::Int8], ret: PgType::Int8 },
            Signature { func: ScalarFn::NumMod, args: &[NUM, NUM], ret: NUM },
        ],
        // money helper functions.
        "cash_words" => &[Signature { func: ScalarFn::CashWords, args: &[MONEY], ret: TEXT }],
        "cashlarger" => &[Signature { func: ScalarFn::CashLarger, args: &[MONEY, MONEY], ret: MONEY }],
        "cashsmaller" => {
            &[Signature { func: ScalarFn::CashSmaller, args: &[MONEY, MONEY], ret: MONEY }]
        }
        "cbrt" => unary_f8!(ScalarFn::Cbrt),
        "exp" => num_and_f8!(ScalarFn::NumExp, ScalarFn::Exp),
        "ln" => num_and_f8!(ScalarFn::NumLn, ScalarFn::Ln),
        // float8 first (an integer/float argument resolves to float8, as in PG);
        // a `numeric` argument still binds the numeric overload exactly. The
        // two-arg `log(base, value)` is numeric-only.
        "log" | "log10" => &[
            Signature { func: ScalarFn::Log10F8, args: &[F8], ret: F8 },
            Signature { func: ScalarFn::NumLog10, args: &[NUM], ret: NUM },
            Signature { func: ScalarFn::NumLog, args: &[NUM, NUM], ret: NUM },
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
            Signature { func: ScalarFn::Power, args: &[F8, F8], ret: F8 },
            Signature { func: ScalarFn::NumPower, args: &[NUM, NUM], ret: NUM },
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
            Signature { func: ScalarFn::DatePart, args: &[TEXT, TS], ret: F8 },
            Signature { func: ScalarFn::DatePartInterval, args: &[TEXT, IV], ret: F8 },
            Signature { func: ScalarFn::DatePartTz, args: &[TEXT, TSTZ], ret: F8 },
            Signature { func: ScalarFn::DatePartDate, args: &[TEXT, DATE], ret: F8 },
            Signature { func: ScalarFn::DatePartTime, args: &[TEXT, TIME], ret: F8 },
            Signature { func: ScalarFn::DatePartTimeTz, args: &[TEXT, TIMETZ], ret: F8 },
        ],
        "date_trunc" => &[
            Signature { func: ScalarFn::DateTrunc, args: &[TEXT, TS], ret: TS },
            Signature { func: ScalarFn::DateTruncInterval, args: &[TEXT, IV], ret: IV },
            Signature { func: ScalarFn::DateTruncTz, args: &[TEXT, TSTZ], ret: TSTZ },
        ],
        "isfinite" => &[
            Signature { func: ScalarFn::Isfinite, args: &[TS], ret: PgType::Bool },
            Signature { func: ScalarFn::IsfiniteInterval, args: &[IV], ret: PgType::Bool },
            Signature { func: ScalarFn::IsfiniteTz, args: &[TSTZ], ret: PgType::Bool },
            Signature { func: ScalarFn::IsfiniteDate, args: &[DATE], ret: PgType::Bool },
        ],
        "make_date" => &[Signature { func: ScalarFn::MakeDate, args: &[I4, I4, I4], ret: DATE }],
        "make_time" => &[Signature { func: ScalarFn::MakeTime, args: &[I4, I4, F8], ret: TIME }],
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
        "justify_days" => &[Signature { func: ScalarFn::JustifyDays, args: &[IV], ret: IV }],
        "justify_hours" => &[Signature { func: ScalarFn::JustifyHours, args: &[IV], ret: IV }],
        "justify_interval" => &[Signature { func: ScalarFn::JustifyInterval, args: &[IV], ret: IV }],
        "age" => &[Signature { func: ScalarFn::Age, args: &[TS, TS], ret: IV }],
        "to_char" => &[Signature { func: ScalarFn::ToCharInterval, args: &[IV, TEXT], ret: TEXT }],
        "make_timestamptz" => &[
            Signature { func: ScalarFn::MakeTimestampTz, args: &[I4, I4, I4, I4, I4, F8], ret: TSTZ },
            Signature {
                func: ScalarFn::MakeTimestampTz,
                args: &[I4, I4, I4, I4, I4, F8, TEXT],
                ret: TSTZ,
            },
        ],
        // The function form of `AT TIME ZONE`: `timezone(zone, value)`.
        "timezone" => &[
            Signature { func: ScalarFn::TimezoneToTz, args: &[TEXT, TS], ret: TSTZ },
            Signature { func: ScalarFn::TimezoneToTs, args: &[TEXT, TSTZ], ret: TS },
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
            Signature { func: ScalarFn::Length, args: &[TEXT], ret: I4 },
            Signature { func: ScalarFn::BitLen, args: &[BIT], ret: I4 },
            Signature { func: ScalarFn::BitLen, args: &[VARBIT], ret: I4 },
        ],
        "char_length" | "character_length" => {
            &[Signature { func: ScalarFn::Length, args: &[TEXT], ret: I4 }]
        }
        // `octet_length` counts the padded bytes of a `bpchar` (via a dedicated
        // overload), while `length`/`bit_length` see the trailing-blank-trimmed
        // text value, matching PG's `bpcharoctetlen` vs `bpcharlen`/text paths.
        "octet_length" => &[
            Signature { func: ScalarFn::OctetLength, args: &[TEXT], ret: I4 },
            Signature { func: ScalarFn::OctetLength, args: &[PgType::Bpchar], ret: I4 },
        ],
        // `bit_length(bit)` is the number of bits, like `length(bit)`.
        "bit_length" => &[
            Signature { func: ScalarFn::BitLength, args: &[TEXT], ret: I4 },
            Signature { func: ScalarFn::BitLen, args: &[BIT], ret: I4 },
            Signature { func: ScalarFn::BitLen, args: &[VARBIT], ret: I4 },
        ],
        "upper" => &[Signature { func: ScalarFn::Upper, args: &[TEXT], ret: TEXT }],
        "lower" => &[Signature { func: ScalarFn::Lower, args: &[TEXT], ret: TEXT }],
        "initcap" => &[Signature { func: ScalarFn::Initcap, args: &[TEXT], ret: TEXT }],
        "substr" | "substring" => &[
            Signature { func: ScalarFn::Substr, args: &[TEXT, I4], ret: TEXT },
            Signature { func: ScalarFn::Substr, args: &[TEXT, I4, I4], ret: TEXT },
            Signature { func: ScalarFn::SubstrBit, args: &[BIT, I4], ret: BIT },
            Signature { func: ScalarFn::SubstrBit, args: &[BIT, I4, I4], ret: BIT },
            Signature { func: ScalarFn::SubstrBit, args: &[VARBIT, I4], ret: VARBIT },
            Signature { func: ScalarFn::SubstrBit, args: &[VARBIT, I4, I4], ret: VARBIT },
        ],
        "strpos" => &[
            Signature { func: ScalarFn::StrPos, args: &[TEXT, TEXT], ret: I4 },
            // `POSITION(bit IN bit)` desugars to `strpos(str, sub)`.
            Signature { func: ScalarFn::BitPosition, args: &[BIT, BIT], ret: I4 },
            Signature { func: ScalarFn::BitPosition, args: &[VARBIT, VARBIT], ret: I4 },
        ],
        "overlay" => &[
            Signature { func: ScalarFn::Overlay, args: &[TEXT, TEXT, I4], ret: TEXT },
            Signature { func: ScalarFn::Overlay, args: &[TEXT, TEXT, I4, I4], ret: TEXT },
            Signature { func: ScalarFn::OverlayBit, args: &[BIT, BIT, I4], ret: BIT },
            Signature { func: ScalarFn::OverlayBit, args: &[BIT, BIT, I4, I4], ret: BIT },
        ],
        "get_bit" => &[
            Signature { func: ScalarFn::GetBit, args: &[BIT, I4], ret: I4 },
            Signature { func: ScalarFn::GetBit, args: &[VARBIT, I4], ret: I4 },
        ],
        "set_bit" => &[
            Signature { func: ScalarFn::SetBit, args: &[BIT, I4, I4], ret: BIT },
            Signature { func: ScalarFn::SetBit, args: &[VARBIT, I4, I4], ret: VARBIT },
        ],
        "bit_count" => &[
            Signature { func: ScalarFn::BitCount, args: &[BIT], ret: I8 },
            Signature { func: ScalarFn::BitCount, args: &[VARBIT], ret: I8 },
        ],
        "ltrim" => &[
            Signature { func: ScalarFn::Ltrim, args: &[TEXT], ret: TEXT },
            Signature { func: ScalarFn::Ltrim, args: &[TEXT, TEXT], ret: TEXT },
        ],
        "rtrim" => &[
            Signature { func: ScalarFn::Rtrim, args: &[TEXT], ret: TEXT },
            Signature { func: ScalarFn::Rtrim, args: &[TEXT, TEXT], ret: TEXT },
        ],
        "btrim" => &[
            Signature { func: ScalarFn::Btrim, args: &[TEXT], ret: TEXT },
            Signature { func: ScalarFn::Btrim, args: &[TEXT, TEXT], ret: TEXT },
        ],
        "lpad" => &[
            Signature { func: ScalarFn::Lpad, args: &[TEXT, I4], ret: TEXT },
            Signature { func: ScalarFn::Lpad, args: &[TEXT, I4, TEXT], ret: TEXT },
        ],
        "rpad" => &[
            Signature { func: ScalarFn::Rpad, args: &[TEXT, I4], ret: TEXT },
            Signature { func: ScalarFn::Rpad, args: &[TEXT, I4, TEXT], ret: TEXT },
        ],
        "replace" => &[Signature { func: ScalarFn::Replace, args: &[TEXT, TEXT, TEXT], ret: TEXT }],
        "translate" => {
            &[Signature { func: ScalarFn::Translate, args: &[TEXT, TEXT, TEXT], ret: TEXT }]
        }
        "repeat" => &[Signature { func: ScalarFn::Repeat, args: &[TEXT, I4], ret: TEXT }],
        "reverse" => &[Signature { func: ScalarFn::Reverse, args: &[TEXT], ret: TEXT }],
        "left" => &[Signature { func: ScalarFn::Left, args: &[TEXT, I4], ret: TEXT }],
        "right" => &[Signature { func: ScalarFn::Right, args: &[TEXT, I4], ret: TEXT }],
        "ascii" => &[Signature { func: ScalarFn::Ascii, args: &[TEXT], ret: I4 }],
        "chr" => &[Signature { func: ScalarFn::Chr, args: &[I4], ret: TEXT }],
        "split_part" => {
            &[Signature { func: ScalarFn::SplitPart, args: &[TEXT, TEXT, I4], ret: TEXT }]
        }
        "starts_with" => &[Signature { func: ScalarFn::StartsWith, args: &[TEXT, TEXT], ret: BOOL }],
        "to_hex" => &[
            Signature { func: ScalarFn::ToHex, args: &[I4], ret: TEXT },
            Signature { func: ScalarFn::ToHexInt8, args: &[I8], ret: TEXT },
        ],
        "encode" => &[Signature { func: ScalarFn::Encode, args: &[BYTEA, TEXT], ret: TEXT }],
        "decode" => &[Signature { func: ScalarFn::Decode, args: &[TEXT, TEXT], ret: BYTEA }],
        "quote_ident" => &[Signature { func: ScalarFn::QuoteIdent, args: &[TEXT], ret: TEXT }],
        "quote_literal" => &[Signature { func: ScalarFn::QuoteLiteral, args: &[TEXT], ret: TEXT }],
        "quote_nullable" => &[Signature { func: ScalarFn::QuoteNullable, args: &[TEXT], ret: TEXT }],
        // inet/cidr accessors. A `cidr` argument coerces to the `inet` overload
        // via the implicit cidr->inet cast, matching PG (whose inet functions
        // accept cidr). `abbrev` keeps a distinct cidr overload because its
        // output differs (`10.1/16` vs `10.1.0.0/16`); the inet overload is
        // listed first so an untyped literal resolves to inet (PG's preferred
        // type in the inet/cidr category), while a typed cidr still binds cidr.
        "host" => &[Signature { func: ScalarFn::Host, args: &[INET], ret: TEXT }],
        "masklen" => &[Signature { func: ScalarFn::Masklen, args: &[INET], ret: I4 }],
        "family" => &[Signature { func: ScalarFn::Family, args: &[INET], ret: I4 }],
        "network" => &[Signature { func: ScalarFn::Network, args: &[INET], ret: CIDR }],
        "abbrev" => &[
            Signature { func: ScalarFn::AbbrevInet, args: &[INET], ret: TEXT },
            Signature { func: ScalarFn::AbbrevCidr, args: &[CIDR], ret: TEXT },
        ],
        // --- geometric constructors / accessors ---
        "point" => &[Signature { func: ScalarFn::Geo(GeoFn::PointConstruct), args: &[F8, F8], ret: POINT }],
        "lseg" => &[Signature { func: ScalarFn::Geo(GeoFn::LsegConstruct), args: &[POINT, POINT], ret: LSEG }],
        "slope" => &[Signature { func: ScalarFn::Geo(GeoFn::PointSlope), args: &[POINT, POINT], ret: F8 }],
        "ishorizontal" => &[Signature { func: ScalarFn::Geo(GeoFn::PointHoriz), args: &[POINT, POINT], ret: BOOL }],
        "isvertical" => &[Signature { func: ScalarFn::Geo(GeoFn::PointVert), args: &[POINT, POINT], ret: BOOL }],
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
        return Ok(Binding::Typed(BoundExpr::FuncCall { func, ret: PgType::Text, args }));
    }

    resolve_call(&name, bindings)
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
    let distinct = matches!(list.duplicate_treatment, Some(ast::DuplicateTreatment::Distinct));
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
            arg: None,
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

    // Every supported aggregate is unary. Bind each argument (an unknown literal
    // resolves to text, as in a bare projection) so a wrong-arity error can name
    // the actual argument types, as PG does.
    let arg_exprs = positional_arg_exprs(&list.args)?;
    let mut bound = arg_exprs
        .iter()
        .map(|e| crate::expr::bind_scalar(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    if bound.len() != 1 {
        let types = bound
            .iter()
            .map(|b| b.ty().name())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!("function {name}({types}) does not exist"),
        ));
    }
    let arg = bound.pop().expect("exactly one argument");
    let input_ty = arg.ty();
    // A DISTINCT aggregate must compare its inputs for equality; a type with no
    // usable equality (e.g. `point`/`lseg`, which are not orderable) reports
    // PG's error rather than reaching the executor's comparison and panicking.
    if distinct && !crate::expr::is_orderable(input_ty) {
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!("could not identify an equality operator for type {}", input_ty.name()),
        ));
    }
    let ret = agg_return_type(agg, input_ty)?;
    Ok(Binding::Typed(BoundExpr::Aggregate {
        func: agg,
        distinct,
        arg: Some(Box::new(arg)),
        input_ty,
        ret,
    }))
}

/// Resolve an overload for `name` given already-bound arguments, then build the
/// `FuncCall` node. Shared by ordinary function calls and the `CEIL`/`FLOOR`
/// special-syntax expressions.
pub(crate) fn resolve_call(name: &str, bindings: Vec<Binding>) -> Result<Binding, BindError> {
    let sigs = lookup(name);
    if sigs.is_empty() {
        return Err(undefined_function(name, &bindings));
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
            return Ok(Binding::Typed(BoundExpr::FuncCall { func: sig.func, ret: sig.ret, args }));
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
        Some((_, sig, args)) => {
            Ok(Binding::Typed(BoundExpr::FuncCall { func: sig.func, ret: sig.ret, args }))
        }
        None => Err(undefined_function(name, &bindings)),
    }
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
    resolve_call(name, vec![arg])
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
    if name != "generate_series" {
        return Ok(None);
    }
    let arg_exprs = positional_args(&func.args)?;
    let bindings = arg_exprs
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    let (elem, args) = resolve_generate_series(&bindings)?;
    Ok(Some(BoundExpr::Srf {
        func: TableFn::GenerateSeries(elem),
        ret: elem,
        args,
    }))
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
    let types = bindings
        .iter()
        .map(crate::expr::binding_type_label)
        .collect::<Vec<_>>()
        .join(", ");
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("function {name}({types}) does not exist"),
    )
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
pub(crate) fn positional_arg_exprs(
    args: &[ast::FunctionArg],
) -> Result<Vec<ast::Expr>, BindError> {
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
