//! Scalar function resolution.
//!
//! Clean-room (see AGENTS.md): the function set, argument coercions, and error
//! text reproduce PG's *observable* behavior, pinned by the vendored regression
//! corpus. Overloads live in a static per-name signature table and are chosen
//! by `resolve_call` with PG's candidate-narrowing rules: drop the signatures
//! the typed arguments cannot reach, keep the most exact matches, prefer a
//! category's preferred type, and only then let the untyped literals decide —
//! reporting `42725` when nothing separates the survivors.

use std::sync::Arc;

use crabgresql_parser::ast;
use crabgresql_parser::ast::Spanned;
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{RoutineImpl, RoutineKind, RoutineSig, TypeCatalog};
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::{PgType, RegKind};

use crate::expr::{
    ArgFail, Binding, BoundExpr, BoundWindowSpec, ExprSortKey, MinMaxKind, Scope, WindowKind,
    bind_expr, bind_sql_function_body, coerce_for_arg, domain_of, inline_params, undomain_binding,
    wrap_domain,
};
use crate::{BindError, BoundAggregate, OutputColumn};

/// A scalar function the executor can evaluate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarFn {
    /// `booleq(boolean, boolean) -> boolean`.
    BoolEq,
    /// `boolne(boolean, boolean) -> boolean`.
    BoolNe,
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
    Sin,
    Cos,
    Tan,
    Cot,
    Asin,
    Acos,
    Atan,
    Atan2,
    Degrees,
    Radians,
    Pi,
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
    /// `sha224(bytea) -> bytea`.
    Sha224,
    /// `sha256(bytea) -> bytea`.
    Sha256,
    /// `sha384(bytea) -> bytea`.
    Sha384,
    /// `sha512(bytea) -> bytea`.
    Sha512,
    /// `crc32(bytea) -> int8`.
    Crc32,
    /// `crc32c(bytea) -> int8`.
    Crc32c,
    /// `date_part(text, timestamp) -> float8`.
    DatePart,
    /// `EXTRACT(field FROM timestamp) -> numeric`; the field is a text arg.
    Extract,
    /// `date_trunc(text, timestamp) -> timestamp`.
    DateTrunc,
    /// `date_bin(interval, timestamp, timestamp) -> timestamp`.
    DateBin,
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
    /// `timestamptz + interval -> timestamptz`. Reads the session zone: the
    /// months and days are calendar quantities, so they move the local wall
    /// clock rather than the instant.
    TimestampTzPlInterval,
    /// `timestamptz - interval -> timestamptz`.
    TimestampTzMiInterval,
    /// `timestamptz - timestamptz -> interval`. Zone-independent, and so a
    /// separate variant from [`ScalarFn::TimestampMi`] only because the
    /// executor's value accessors are typed by `Value` variant.
    TimestampTzMi,

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
    /// `age(timestamptz, timestamptz) -> interval`; the symbolic difference of
    /// the two session-zone wall clocks.
    AgeTz,
    /// `age(timestamp) -> interval` = `age(current_date::timestamp, $1)`.
    AgeToday,
    /// `age(timestamptz) -> interval` = `age(current_date::timestamptz, $1)`.
    AgeTodayTz,
    /// `interval_in(text, int4)`: parse an interval literal at execution time,
    /// under the session's `IntervalStyle`, with the second argument carrying
    /// the leading-field default unit. Only reached for a literal whose meaning
    /// the style can change (see `interval::style_sensitive`).
    IntervalIn,
    /// `age(xid) -> int4`: how many transactions have started since `xid`.
    /// Reads the live transaction counter, so it is dispatched from `eval.rs`
    /// rather than from the pure `eval_scalar`.
    AgeXid,
    /// The current transaction's id: `txid_current()`, its modern spelling
    /// `pg_current_xact_id()` (`xid8`), and the `_if_assigned` form of each.
    ///
    /// Only the plain form assigns an id when the transaction has none, which
    /// is why [`crate::plan_needs_xid`] looks for it: the server allocates the
    /// XID before execution starts.
    CurrentXactId {
        xid8: bool,
        if_assigned: bool,
    },
    /// `pg_xact_status(xid8) -> text`: `committed`, `aborted` or `in progress`,
    /// read out of the CLOG. NULL for an XID too old to have a status left.
    PgXactStatus,
    /// `pg_is_in_recovery() -> bool`: always false, see its arm in `eval.rs`.
    PgIsInRecovery,
    /// `to_char(interval, text) -> text`.
    ToCharInterval,
    /// `to_char(timestamp, text) -> text`.
    ToCharTimestamp,
    /// `to_char(timestamptz, text) -> text`.
    ToCharTimestampTz,
    /// `to_char(time, text) -> text`; PG reaches this through its implicit
    /// `time -> interval` cast, so it renders with the interval codes.
    ToCharTime,
    /// `to_char(numeric, text) -> text`.
    ToCharNumeric,
    /// `to_char(int4, text) -> text`.
    ToCharInt4,
    /// `to_char(int8, text) -> text`.
    ToCharInt8,
    /// `to_char(float4, text) -> text`.
    ToCharFloat4,
    /// `to_char(float8, text) -> text`.
    ToCharFloat8,
    /// `to_date(text, text) -> date`.
    ToDate,
    /// `to_timestamp(text, text) -> timestamptz`.
    ToTimestampFormat,
    /// `to_timestamp(float8) -> timestamptz`: seconds since the Unix epoch.
    ToTimestampUnix,
    /// `to_number(text, text) -> numeric`.
    ToNumber,

    // --- timestamptz operators/functions ---
    /// `date_part(text, timestamptz) -> float8`.
    DatePartTz,
    /// `EXTRACT(field FROM timestamptz) -> numeric`; the field is a text arg.
    ExtractTz,
    /// `date_trunc(text, timestamptz[, text]) -> timestamptz`. The optional
    /// third argument names the zone to truncate in; without it the session zone
    /// is used.
    DateTruncTz,
    /// `date_bin(interval, timestamptz, timestamptz) -> timestamptz`.
    DateBinTz,
    /// `pg_typeof(any) -> regtype`, carrying the OID of the argument's static
    /// type. The argument stays in `args` and is still evaluated for its errors
    /// and side effects; only its *value* is unused. Not STRICT —
    /// `pg_typeof(NULL)` is `unknown`, not NULL — and it needs the catalog to
    /// name a user type, so `eval` dispatches it and the pure evaluator rejects
    /// it.
    PgTypeof(u32),
    /// `isfinite(timestamptz) -> bool`.
    IsfiniteTz,
    /// `make_timestamptz(int×5, float8[, text]) -> timestamptz`.
    MakeTimestampTz,
    /// `current_setting(text[, bool]) -> text`. Reads the session GUC table via
    /// [`crabgresql_executor::GucOps`], so it dispatches in `eval`, not in the
    /// pure `eval_scalar`.
    CurrentSetting,
    /// `version() -> text`. A build-time constant, so the pure `eval_scalar`
    /// answers it — no session state is involved.
    Version,

    // --- the clock. All four answer with an instant the session or the process
    // carries rather than with anything from their arguments, so they dispatch
    // in `eval`, not in `eval_scalar`.
    /// `now()` / `transaction_timestamp() -> timestamptz`: when the current
    /// transaction started. Stable — the same value for every row and every
    /// statement of the block. Also what a `'now'` literal resolves to.
    TransactionTimestamp,
    /// `statement_timestamp() -> timestamptz`: when the current protocol
    /// message arrived. Stable within it, so a multi-statement simple query
    /// sees one value throughout.
    StatementTimestamp,
    /// `clock_timestamp() -> timestamptz`: the wall clock, read afresh at every
    /// call. The only volatile member of the family.
    ClockTimestamp,
    /// `pg_postmaster_start_time() -> timestamptz`: when this server process
    /// started. Fixed for the life of the process, so every session and every
    /// row sees the same instant.
    PgPostmasterStartTime,

    // --- uuid generation. Volatile and clock-reading like the family above, so
    // these dispatch in `eval` too; the extractors below are pure and do not.
    /// `gen_random_uuid() -> uuid`, and its RFC-spelled alias `uuidv4()`:
    /// 122 random bits. PG gives both names one implementation, so we do too.
    GenRandomUuid,
    /// `uuidv7() -> uuid`: the current instant, then randomness. Successive
    /// calls sort in generation order.
    UuidV7,
    /// `uuidv7(interval) -> uuid`: as above, but stamped with the current
    /// instant shifted by the argument.
    UuidV7Shift,
    /// `uuid_extract_version(uuid) -> int2`: the version nibble, or NULL when
    /// the value is not an RFC 9562 variant. Immutable.
    UuidExtractVersion,
    /// `uuid_extract_timestamp(uuid) -> timestamptz`: the instant a version 1
    /// or version 7 value carries, NULL for any other. Immutable.
    UuidExtractTimestamp,

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
    /// `timezone(text, timetz) -> timetz` (`timetz AT TIME ZONE zone`).
    TimezoneTimeTz,
    /// `timezone(interval, timetz) -> timetz` (`timetz AT TIME ZONE INTERVAL …`).
    TimezoneIntervalTimeTz,
    /// `timezone(timetz) -> timetz` (`timetz AT LOCAL`): the session zone.
    TimezoneLocalTimeTz,
    /// `timezone(timestamp) -> timestamptz` (`timestamp AT LOCAL`).
    TimezoneLocalToTz,
    /// `timezone(timestamptz) -> timestamp` (`timestamptz AT LOCAL`).
    TimezoneLocalToTs,
    /// `timezone(interval, timestamp) -> timestamptz`.
    TimezoneIntervalToTz,
    /// `timezone(interval, timestamptz) -> timestamp`.
    TimezoneIntervalToTs,

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
    /// Apply a fractional-second precision modifier at run time. Args are
    /// `(time|timetz|timestamp|timestamptz, int4 precision)`; one function
    /// serves all four because they round the same way and differ only in which
    /// `Value` carries the microseconds.
    TimeApplyTypmod,
    /// Apply an `interval` type modifier at run time. Args are `(interval, int4
    /// typmod)` — the *packed* modifier, since an interval's admitted fields and
    /// its precision travel together.
    IntervalTypmod,
    /// `abs(float8) -> float8`.
    AbsF8,
    /// `log(float8) -> float8` (base 10).
    Log10F8,
    /// `mod(intN, intN) -> intN` (dispatches on the operand's integer width).
    ModInt,
    /// `abs(int2|int4|int8|float4)` returning the argument's own type
    /// (dispatches on the operand's width, as [`ScalarFn::ModInt`] does).
    /// `abs(numeric)` and `abs(float8)` have their own entries above.
    AbsExact,
    /// `gcd(intN, intN) -> intN` (dispatches on the operand's integer width).
    GcdInt,
    /// `lcm(intN, intN) -> intN` (dispatches on the operand's integer width).
    LcmInt,
    /// `gcd(numeric, numeric) -> numeric`.
    NumGcd,
    /// `lcm(numeric, numeric) -> numeric`.
    NumLcm,

    // --- string functions (see `crabgresql_types::text`) ----
    /// `text || text -> text` (the `||` operator / `textcat`).
    TextConcat,
    /// `length`/`char_length`/`character_length(text) -> int4`, and
    /// `length(bytea) -> int4`, which counts bytes rather than characters.
    Length,
    /// `octet_length(text|bpchar|bytea) -> int4`.
    OctetLength,
    /// `bit_length(text|bytea) -> int4`.
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
    /// The `f(… , VARIADIC arr)` spellings of the three above: the **last**
    /// argument is an array whose elements stand in for the trailing arguments,
    /// expanded at run time because only then is its length known.
    ///
    /// They are separate variants rather than a flag because the NULL rules
    /// differ from the spread-out call: `concat(VARIADIC NULL::int[])` is NULL
    /// where `concat(NULL)` is the empty string, and
    /// `format('%s', VARIADIC NULL::text[])` is "too few arguments" where
    /// `format('%s', NULL)` is the empty string.
    ConcatVariadic,
    /// `concat_ws(sep, VARIADIC arr)`.
    ConcatWsVariadic,
    /// `format(picture, VARIADIC arr)`.
    FormatVariadic,
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
    /// `name` input: clip to 63 bytes (`text`).
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
    // --- integer bitwise / shift (int2 / int4 / int8) ---
    // Each takes and returns the *left* operand's integer type, so one variant
    // covers all three widths; the executor dispatches on the `Value`.
    /// `intN << int4` — the count is int4 for every width, and PG applies no
    /// overflow check: `(-1)::int2 << 15` is -32768.
    IntShl,
    /// `intN >> int4` — arithmetic (sign-propagating) shift right.
    IntShr,
    /// `intN & intN` (bitwise AND), both operands the same width.
    IntAnd,
    /// `intN | intN` (bitwise OR), both operands the same width.
    IntOr,
    /// `intN # intN` (bitwise XOR), both operands the same width.
    IntXor,
    /// `~intN` (one's complement).
    IntNot,
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
    // --- geometric (point / lseg / path / box / line / circle / polygon) ---
    /// A geometric operator/function; the specific operation is the payload.
    Geo(GeoFn),
    // --- tid ---
    /// `tid_block(tid) -> int8`: the block number half of a tuple identifier.
    /// `int8`, not `int4`, because a `BlockNumber` is an unsigned 32-bit value.
    TidBlock,
    /// `tid_offset(tid) -> int4`: the offset half of a tuple identifier.
    TidOffset,
    // --- xid8 ---
    /// `xid8cmp(xid8, xid8) -> int4`: the btree three-way comparison, exposed
    /// as an ordinary function. `xid` has no counterpart — it has no btree
    /// opclass at all.
    Xid8Cmp,
    // --- pg_lsn (the binder normalizes the commuted spellings, so wherever an
    // LSN is an operand it is argument 0) ---
    /// `pg_lsn - pg_lsn -> numeric`: the exact signed byte distance.
    PgLsnMi,
    /// `pg_lsn + numeric -> pg_lsn` (and the commuted `numeric + pg_lsn`).
    PgLsnPli,
    /// `pg_lsn - numeric -> pg_lsn`.
    PgLsnMii,
    /// `pg_lsn(numeric) -> pg_lsn`: the explicit conversion function.
    NumericPgLsn,
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
    /// `current_database() -> name`, and the `current_catalog` keyword.
    CurrentDatabase,
    /// `current_schema[()] -> name`: the head of the explicit search path.
    CurrentSchema,
    /// `current_schemas(bool) -> name[]`: the search path, optionally including
    /// the implicit `pg_catalog` and temp entries.
    CurrentSchemas,
    /// `current_user -> name`, and its `current_role` / `user` spellings. One
    /// variant for all three because PostgreSQL has one `pg_proc` row: the other
    /// two are grammar that rewrites to it.
    CurrentUser,
    /// `session_user -> name`.
    SessionUser,
    /// `pg_my_temp_schema() -> oid`: this session's temp namespace, or 0 before
    /// a temp relation instantiates it.
    PgMyTempSchema,
    /// `pg_is_other_temp_schema(oid) -> bool`: whether the OID names *another*
    /// session's temp namespace.
    PgIsOtherTempSchema,
    /// `pg_backend_pid() -> int4`: this connection's backend id, which is what
    /// `pg_locks.pid` reports and what the client was given in `BackendKeyData`.
    PgBackendPid,
    /// `pg_encoding_to_char(int4) -> name`: the name of the encoding numbered
    /// `n`, or the empty string past the end of PostgreSQL's fixed table.
    PgEncodingToChar,
    /// `pg_char_to_encoding(name) -> int4`: the inverse, `-1` for a name no
    /// encoding answers to.
    PgCharToEncoding,
    /// `pg_tablespace_location(oid) -> text`: where a tablespace's directory
    /// lives. The empty string for the two bootstrap tablespaces (which sit
    /// inside the data directory rather than beside it), and an error for every
    /// other OID — PostgreSQL stats `pg_tblspc/<oid>` and reports what it finds,
    /// so this never consults `pg_tablespace`.
    PgTablespaceLocation,
    /// `pg_indexam_has_property(oid, text) -> bool`: whether an index access
    /// method supports a capability (`can_order`, `can_unique`, ...). NULL for a
    /// property the AM level does not answer for, and for an OID that is not an
    /// index AM.
    PgIndexamHasProperty,
    /// `pg_index_has_property(regclass, text) -> bool`: the same question about a
    /// whole index (`clusterable`, `index_scan`, ...). NULL for anything that is
    /// not an index.
    PgIndexHasProperty,
    /// `pg_index_column_has_property(regclass, int4, text) -> bool`: the same
    /// about one key column of an index (`asc`, `nulls_first`, `returnable`,
    /// ...), numbered from 1. NULL past the end of the key list.
    PgIndexColumnHasProperty,
    /// `pg_table_is_visible(oid) -> bool`: whether the relation is reachable by
    /// an unqualified name. NULL for an OID no relation has.
    PgTableIsVisible,
    /// `pg_relation_size(regclass[, text]) -> int8`: the relation's own storage
    /// in bytes. The second argument names a fork (`main`, `fsm`, `vm`, `init`)
    /// and defaults to `main`; anything else is `22023`. NULL for an OID no
    /// relation has, and **zero** for a relation with no storage of its own — a
    /// view or a partitioned parent.
    PgRelationSize,
    /// `pg_table_size(regclass) -> int8`: the relation plus its out-of-line
    /// storage, but not its indexes.
    PgTableSize,
    /// `pg_indexes_size(regclass) -> int8`: every index on the relation, summed.
    /// Zero for a relation that is itself an index.
    PgIndexesSize,
    /// `pg_total_relation_size(regclass) -> int8`: the relation, its out-of-line
    /// storage and its indexes together.
    PgTotalRelationSize,
    /// `pg_size_pretty(int8|numeric) -> text`: a byte count in the largest unit
    /// that leaves it under `10 * 1024` — `bytes`, `kB`, `MB`, `GB`, `TB`, `PB`.
    /// Pure: it reads no catalog, only its argument.
    PgSizePretty,
    /// `obj_description(oid[, name]) -> text`: the comment on an object, from
    /// `pg_description`. With the catalog name it looks only there; the
    /// one-argument form is PostgreSQL's deprecated any-catalog search, which
    /// raises `21000` if two catalogs both describe the OID. A catalog name no
    /// `pg_catalog` relation answers to is NULL, not an error — upstream's body
    /// finds the `classoid` with a sub-select, and a sub-select with no rows is
    /// NULL.
    ObjDescription,
    /// `col_description(oid, int4) -> text`: the comment on one column of a
    /// relation — the same lookup with a non-zero `objsubid`.
    ColDescription,
    /// The `tableoid` system column: the OID of the relation a row came from.
    /// Its two arguments are the relation's namespace and name as text literals,
    /// resolved through the catalog at *execution* time rather than folded here
    /// — relation OIDs are positional over the catalog snapshot, so a prepared
    /// statement holding a frozen one would go stale the moment another relation
    /// is created or dropped ahead of it. Emitted only by name resolution
    /// ([`crate::expr::Scope`]), never callable by name, as in PostgreSQL.
    TableOid,
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
    /// `pg_get_viewdef(text|oid[, bool|int4]) -> text`: the view's `SELECT`,
    /// re-rendered in PostgreSQL's canonical shape by [`crate::ruleutils`].
    PgGetViewdef,
    /// `pg_get_ruledef(oid[, bool]) -> text`: a rewrite rule's `CREATE RULE`
    /// statement. Every rule here is a view's `_RETURN`, so the body is the
    /// view's own definition and the flag is [`PgGetViewdef`](Self::PgGetViewdef)'s.
    /// An OID no rule answers to is NULL, not an error.
    PgGetRuledef,
    /// `pg_get_triggerdef(oid[, bool]) -> text`: a trigger's `CREATE TRIGGER`
    /// statement, always NULL while `pg_trigger` is empty.
    PgGetTriggerdef,
    /// `pg_get_function_arguments(oid) -> text`: a function's argument list as
    /// `CREATE FUNCTION` would take it (`a integer, OUT b text`).
    PgGetFunctionArguments,
    /// `pg_get_function_identity_arguments(oid) -> text`: the same list with the
    /// argument defaults left off — what `DROP FUNCTION` needs.
    PgGetFunctionIdentityArguments,
    /// `pg_get_function_result(oid) -> text`: a function's `RETURNS` clause,
    /// `SETOF`/`TABLE(...)` included.
    PgGetFunctionResult,
    /// `pg_get_indexdef(oid[, int4, bool]) -> text`: an index's `CREATE INDEX`
    /// DDL, rendered by [`crabgresql_storage_api::index_definition`]. The
    /// three-argument form is PostgreSQL's per-column one: a non-zero column
    /// number yields that key alone rather than the whole statement. An OID no
    /// index answers to is NULL, not an error.
    PgGetIndexdef,
    /// `pg_get_partkeydef(oid) -> text`: the argument of a partitioned table's
    /// `PARTITION BY` clause, as `RANGE (sales_date)`. An OID that names no
    /// partitioned relation — including one that names nothing at all — is
    /// NULL, not an error.
    PgGetPartkeydef,
    /// `pg_get_constraintdef(oid[, bool]) -> text`: a constraint's DDL, as
    /// `CHECK ((x > 3))` / `PRIMARY KEY (a, b)` / `UNIQUE (a)`. The optional
    /// flag is PostgreSQL's `pretty`, which for a check drops the parentheses
    /// `pg_get_expr` adds. An OID no constraint answers to is NULL, not an
    /// error.
    PgGetConstraintdef,
    /// `pg_get_serial_sequence(text, text) -> text`: the sequence a `serial`
    /// column owns, schema-qualified, or NULL when the column owns none. The
    /// relation argument is a possibly-qualified relation *name*; the column
    /// argument is taken literally, as PostgreSQL takes it.
    PgGetSerialSequence,
    // --- jsonpath (jsonb @ jsonpath) ---
    /// A `jsonb_path_*` function / `@?` / `@@` operator. Args are
    /// `[jsonb, jsonpath]` optionally followed by `[vars jsonb, silent bool]`;
    /// the `@?`/`@@` operators pass a `silent = true` 4th arg.
    JsonPath(JsonPathFn),
    // --- json / jsonb extraction operators ---
    /// A `->` / `->>` / `#>` / `#>>` extraction. Args are `[json|jsonb, key]`,
    /// where the key is `text` (object field), `int4` (array element) or
    /// `text[]` (path). The `json` vs `jsonb` behavior is selected at eval time
    /// from the target's `Value` variant.
    Json(JsonFn),
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

/// A JSON extraction operator. Each applies to both `json` and `jsonb`; the
/// target's `Value` variant picks the behavior, so one set of variants covers
/// both types. The `*Text` forms are the `->>`/`#>>` spellings, which return
/// `text` (a JSON string unquoted, a JSON `null` as SQL NULL).
///
/// Named after PG's underlying functions so that registering a SQL spelling is
/// a lookup-table entry and nothing more.
///
/// TODO: register the SQL spellings of these extractions
/// (`jsonb_extract_path`, `json_object_field`, ...); only the operator forms
/// resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonFn {
    /// `json -> text` — an object field.
    ObjectField,
    /// `json ->> text` — an object field, as text.
    ObjectFieldText,
    /// `json -> int4` — an array element; a negative subscript counts from the end.
    ArrayElement,
    /// `json ->> int4` — an array element, as text.
    ArrayElementText,
    /// `json #> text[]` — the value at a path.
    ExtractPath,
    /// `json #>> text[]` — the value at a path, as text.
    ExtractPathText,
}

/// A text-search operation over `tsvector`/`tsquery`. Operators lower to these
/// via `resolve_ts_op` (`@@`, `&&`, `<->`), `resolve_ts_concat` (`||`) and
/// `resolve_ts_unary` (`!!`); named functions register them in [`lookup`].
/// Argument order is fixed per variant.
///
/// PG spells the weight arguments as `"char"` and `"char"[]`; they are modeled
/// as `text`/`text[]`, and `"char" -> text` is an implicit cast, so both a
/// literal like `setweight(v, 'c')` and a `"char"`-typed argument bind. Only
/// the first character is read, as the `"char"` cast would.
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

/// A geometric (`point` / `lseg` / `path` / `box` / `line` / `circle` /
/// `polygon`) operation. Operators lower to these via
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
    /// `isopen(path) -> bool`.
    PathIsOpen,
    /// `isclosed(path) -> bool`.
    PathIsClosed,
    /// `popen(path) -> path`.
    PathPopen,
    /// `pclose(path) -> path`.
    PathPclose,
    /// `# path` / `npoints(path) -> int4`.
    PathNpoints,
    /// `@-@ path` / `length(path) -> float8`.
    PathLength,
    /// `area(path) -> float8` (NULL for an open path).
    PathArea,
    /// `p1 + p2` path concatenation (`-> path`, NULL if either is closed).
    PathConcat,
    /// `path + point` translate (`-> path`).
    PathAddPt,
    /// `path - point` translate (`-> path`).
    PathSubPt,
    /// `path * point` rotate / scale (`-> path`).
    PathMulPt,
    /// `path / point` rotate / scale (`-> path`).
    PathDivPt,
    /// `p1 <-> p2` path distance (`-> float8`, NULL if neither has a segment).
    PathDist,
    /// `path <-> point` distance (`-> float8`); args are `[path, point]`.
    DistPathPoint,
    /// `point <@ path` (on the outline, or inside a closed path); args are
    /// `[point, path]`.
    OnPpath,
    /// `path @> point` (inside or on a closed path); args are `[path, point]`.
    PathContainPt,
    /// `p1 ?# p2` (their outlines cross).
    PathInter,
    /// `p1 = p2` (equal point counts).
    PathEq,
    /// `p1 <> p2`.
    PathNe,
    /// `p1 < p2` (by point count).
    PathLt,
    /// `p1 <= p2` (by point count).
    PathLe,
    /// `p1 > p2` (by point count).
    PathGt,
    /// `p1 >= p2` (by point count).
    PathGe,

    // -- box ---------------------------------------------------------------
    /// `box(point) -> box` / `point::box` (a degenerate box).
    BoxFromPoint,
    /// `box(point, point) -> box`.
    BoxConstruct,
    /// `area(box) -> float8`.
    BoxArea,
    /// `width(box) -> float8`.
    BoxWidth,
    /// `height(box) -> float8`.
    BoxHeight,
    /// `@@ box` / `center(box)` / `box::point` (`-> point`).
    BoxCenter,
    /// `diagonal(box)` / `lseg(box)` / `box::lseg` (`-> lseg`).
    BoxDiagonal,
    /// `bound_box(box, box) -> box`.
    BoundBox,
    /// `b1 && b2` overlap.
    BoxOverlap,
    /// `b1 << b2` (strictly left).
    BoxLeft,
    /// `b1 >> b2` (strictly right).
    BoxRight,
    /// `b1 &< b2` (does not extend right of).
    BoxOverLeft,
    /// `b1 &> b2` (does not extend left of).
    BoxOverRight,
    /// `b1 <<| b2` (strictly below).
    BoxBelow,
    /// `b1 |>> b2` (strictly above).
    BoxAbove,
    /// `b1 &<| b2` (does not extend above).
    BoxOverBelow,
    /// `b1 |&> b2` (does not extend below).
    BoxOverAbove,
    /// `b1 <^ b2` (is below, touching allowed).
    BoxBelowEq,
    /// `b1 >^ b2` (is above, touching allowed).
    BoxAboveEq,
    /// `b1 @> b2` contains.
    BoxContain,
    /// `b1 <@ b2` contained in.
    BoxContained,
    /// `b1 ~= b2` same as (identical corners).
    BoxSame,
    /// `b1 ?# b2` (they share a point).
    BoxIntersects,
    /// `b1 # b2` intersection box (`-> box`, NULL if disjoint).
    BoxIntersect,
    /// `b1 = b2` (by area — identity is `~=`). PG gives `box` no `<>`
    /// counterpart, so there is no `BoxNe`.
    BoxEq,
    /// `b1 < b2` (by area).
    BoxLt,
    /// `b1 <= b2` (by area).
    BoxLe,
    /// `b1 > b2` (by area).
    BoxGt,
    /// `b1 >= b2` (by area).
    BoxGe,
    /// `box + point` (`-> box`).
    BoxAddPt,
    /// `box - point` (`-> box`).
    BoxSubPt,
    /// `box * point` (`-> box`).
    BoxMulPt,
    /// `box / point` (`-> box`).
    BoxDivPt,
    /// `box @> point` / `point <@ box`; args are `[box, point]`.
    BoxContainPt,
    /// `point ## box` closest point (`-> point`); args are `[point, box]`.
    ClosePointBox,
    /// `point <-> box` distance (`-> float8`); args are `[point, box]`.
    DistPointBox,
    /// `lseg <@ box`; args are `[lseg, box]`.
    LsegInsideBox,
    /// `lseg ?# box`; args are `[lseg, box]`.
    LsegIntersectsBox,
    /// `lseg <-> box` distance (`-> float8`); args are `[lseg, box]`.
    DistLsegBox,
    /// `lseg ## box` closest point (`-> point`); args are `[lseg, box]`.
    CloseLsegBox,
    /// `b1 <-> b2` box distance — **center to center** (`-> float8`).
    DistBoxBox,
    /// `box::circle` / `circle(box)` (`-> circle`).
    BoxToCircle,
    /// `box::polygon` / `polygon(box)` (`-> polygon`).
    BoxToPolygon,

    // -- line --------------------------------------------------------------
    /// `line(point, point) -> line`.
    LineConstruct,
    /// `l1 = l2` (scale invariant, NaN-exact).
    LineEq,
    /// `?- line` / `ishorizontal(line)`.
    LineHoriz,
    /// `?| line` / `isvertical(line)`.
    LineVert,
    /// `l1 ?|| l2` / `isparallel(line, line)`.
    LineParallel,
    /// `l1 ?-| l2` / `isperp(line, line)`.
    LinePerpendicular,
    /// `l1 # l2` intersection point (`-> point`, NULL if parallel).
    LineInterpt,
    /// `l1 ?# l2` (they meet in one point).
    LineIntersects,
    /// `l1 <-> l2` distance (`-> float8`).
    DistLineLine,
    /// `point <-> line` distance (`-> float8`); args are `[point, line]`.
    DistPointLine,
    /// `point ## line` foot of the perpendicular (`-> point`); args are
    /// `[point, line]`.
    ClosePointLine,
    /// `point <@ line`; args are `[point, line]`.
    PointOnLine,
    /// `lseg <@ line`; args are `[lseg, line]`.
    LsegOnLine,
    /// `lseg ?# line`; args are `[lseg, line]`.
    LsegIntersectsLine,
    /// `lseg <-> line` distance (`-> float8`); args are `[lseg, line]`.
    DistLsegLine,
    /// `line ## lseg` closest point on the segment (`-> point`, NULL if
    /// parallel); args are `[line, lseg]`.
    CloseLineLseg,
    /// `line ?# box`; args are `[line, box]`.
    LineIntersectsBox,

    // -- circle ------------------------------------------------------------
    /// `circle(point, float8) -> circle`.
    CircleConstruct,
    /// `@@ circle` / `center(circle)` / `point(circle)` / `circle::point`.
    CircleCenter,
    /// `radius(circle) -> float8`.
    CircleRadius,
    /// `diameter(circle) -> float8`.
    CircleDiameter,
    /// `area(circle) -> float8`.
    CircleArea,
    /// `circle::box` / `box(circle)` (`-> box`).
    CircleToBox,
    /// `circle(polygon) -> circle`.
    CircleFromPolygon,
    /// `circle::polygon` / `polygon(circle)` (`-> polygon`, 12 points).
    CircleToPolygon,
    /// `polygon(int4, circle) -> polygon`; args are `[int4, circle]`.
    CircleToPolygonN,
    /// `c1 ~= c2` same as.
    CircleSame,
    /// `c1 && c2` overlap.
    CircleOverlap,
    /// `c1 << c2` (strictly left).
    CircleLeft,
    /// `c1 >> c2` (strictly right).
    CircleRight,
    /// `c1 &< c2` (does not extend right of).
    CircleOverLeft,
    /// `c1 &> c2` (does not extend left of).
    CircleOverRight,
    /// `c1 <<| c2` (strictly below).
    CircleBelow,
    /// `c1 |>> c2` (strictly above).
    CircleAbove,
    /// `c1 &<| c2` (does not extend above).
    CircleOverBelow,
    /// `c1 |&> c2` (does not extend below).
    CircleOverAbove,
    /// `c1 @> c2` contains.
    CircleContain,
    /// `c1 <@ c2` contained in.
    CircleContained,
    /// `circle @> point` / `point <@ circle`; args are `[circle, point]`.
    CircleContainPt,
    /// `pt_contained_circle(point, circle)` — the same test with the arguments
    /// in the other order, which is how PG spells the function.
    CircleContainPtSwapped,
    /// `c1 = c2` (by area — identity is `~=`).
    CircleEq,
    /// `c1 <> c2` (by area).
    CircleNe,
    /// `c1 < c2` (by area).
    CircleLt,
    /// `c1 <= c2` (by area).
    CircleLe,
    /// `c1 > c2` (by area).
    CircleGt,
    /// `c1 >= c2` (by area).
    CircleGe,
    /// `c1 <-> c2` distance (`-> float8`).
    DistCircleCircle,
    /// `point <-> circle` distance (`-> float8`); args are `[point, circle]`.
    DistPointCircle,
    /// `circle + point` (`-> circle`).
    CircleAddPt,
    /// `circle - point` (`-> circle`).
    CircleSubPt,
    /// `circle * point` (`-> circle`).
    CircleMulPt,
    /// `circle / point` (`-> circle`).
    CircleDivPt,

    // -- polygon -----------------------------------------------------------
    /// `# polygon` / `npoints(polygon) -> int4`.
    PolyNpoints,
    /// `@@ polygon` / `point(polygon)` / `polygon::point` (`-> point`).
    PolyCenter,
    /// `polygon::box` / `box(polygon)` (`-> box`).
    PolyToBox,
    /// `polygon::path` / `path(polygon)` (`-> path`, always closed).
    PolyToPath,
    /// `path::polygon` / `polygon(path)` (`-> polygon`; an open path errors).
    PathToPolygon,
    /// `p1 ~= p2` same as.
    PolySame,
    /// `p1 && p2` overlap.
    PolyOverlap,
    /// `p1 << p2` (strictly left).
    PolyLeft,
    /// `p1 >> p2` (strictly right).
    PolyRight,
    /// `p1 &< p2` (does not extend right of).
    PolyOverLeft,
    /// `p1 &> p2` (does not extend left of).
    PolyOverRight,
    /// `p1 <<| p2` (strictly below).
    PolyBelow,
    /// `p1 |>> p2` (strictly above).
    PolyAbove,
    /// `p1 &<| p2` (does not extend above).
    PolyOverBelow,
    /// `p1 |&> p2` (does not extend below).
    PolyOverAbove,
    /// `p1 @> p2` contains.
    PolyContain,
    /// `p1 <@ p2` contained in.
    PolyContained,
    /// `polygon @> point` / `point <@ polygon`; args are `[polygon, point]`.
    PolyContainPt,
    /// `pt_contained_poly(point, polygon)` — the same test with the arguments
    /// in the other order.
    PolyContainPtSwapped,
    /// `p1 <-> p2` polygon distance (`-> float8`).
    DistPolyPoly,
    /// `polygon <-> point` distance (`-> float8`); args are `[polygon, point]`.
    DistPolyPoint,
    /// `polygon <-> circle` distance (`-> float8`); args are
    /// `[polygon, circle]`.
    DistPolyCircle,
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
    /// `generate_series(start, stop [, step])`. The element type carried here —
    /// `int4`, `int8`, `numeric`, or a `timestamp`/`timestamptz` stepped by an
    /// interval — is the output column's. Yields one row per value in the range.
    GenerateSeries(PgType),
    /// `jsonb_path_query(target jsonb, path jsonpath [, vars jsonb, silent bool])`
    /// — one `jsonb` row per item the path returns.
    JsonbPathQuery,
    /// `unnest(array)` over a 1-D array whose element type is carried here. Yields
    /// one row per element (NULL elements included).
    Unnest(PgType),
    /// `generate_subscripts(array, dim [, reverse])` — the valid subscripts of
    /// the array's `dim`th dimension, as `int4`. `reverse` yields them
    /// descending. A dimension the array does not have (and any NULL argument)
    /// yields no rows rather than an error.
    GenerateSubscripts,
    /// `pg_available_extensions()` — one row per installable extension, as
    /// `(name, default_version, comment)`. psql's `\dx` calls the function
    /// rather than the view of the same name, so both have to exist.
    PgAvailableExtensions,
    /// `pg_available_extension_versions()` — one row per installable
    /// *(extension, version)* pair, with the flags that version would be
    /// installed under. Nine columns in the view of this name, eight here: the
    /// function does not report `installed`, which the view computes.
    PgAvailableExtensionVersions,
    /// `pg_partition_ancestors(regclass)` — the relation itself, then each
    /// partitioned parent up to the root. A relation that is neither a partition
    /// nor partitioned yields **no rows** (observed on 18.4), which is what
    /// makes psql's `\d` footers join against it unconditionally.
    PgPartitionAncestors,
}

impl TableFn {
    /// The function's declared parameter types (for arity/coercion checks).
    /// `GenerateSeries` is polymorphic in its element type (2- or 3-arg) and
    /// resolves via [`resolve_generate_series`] instead, so it has no fixed
    /// signature here.
    fn arg_types(self) -> &'static [PgType] {
        match self {
            TableFn::PgInputErrorInfo => &[PgType::Text, PgType::Text],
            // `PgPartitionAncestors` accepts either `regclass` or `oid` and so
            // resolves in `resolve_partition_ancestors`, like the polymorphic
            // ones above.
            TableFn::PgPartitionAncestors => &[],
            TableFn::PgAvailableExtensions | TableFn::PgAvailableExtensionVersions => &[],
            // The polymorphic/variadic ones resolve their own arguments in
            // `bind_table_fn_call`.
            TableFn::GenerateSeries(_)
            | TableFn::JsonbPathQuery
            | TableFn::Unnest(_)
            | TableFn::GenerateSubscripts => &[],
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
            TableFn::GenerateSeries(elem) => vec![OutputColumn::new("generate_series", elem)],
            TableFn::JsonbPathQuery => {
                vec![OutputColumn::new("jsonb_path_query", PgType::Jsonb)]
            }
            TableFn::Unnest(elem) => vec![OutputColumn::new("unnest", elem)],
            TableFn::GenerateSubscripts => {
                vec![OutputColumn::new("generate_subscripts", PgType::Int4)]
            }
            TableFn::PgPartitionAncestors => {
                vec![OutputColumn::new("relid", PgType::Reg(RegKind::Class))]
            }
            TableFn::PgAvailableExtensions => vec![
                OutputColumn::new("name", PgType::Name),
                OutputColumn::new("default_version", PgType::Text),
                OutputColumn::new("comment", PgType::Text),
            ],
            TableFn::PgAvailableExtensionVersions => vec![
                OutputColumn::new("name", PgType::Name),
                OutputColumn::new("version", PgType::Text),
                OutputColumn::new("superuser", PgType::Bool),
                OutputColumn::new("trusted", PgType::Bool),
                OutputColumn::new("relocatable", PgType::Bool),
                OutputColumn::new("schema", PgType::Name),
                OutputColumn::new("requires", PgType::Array(crabgresql_types::oid::NAME)),
                OutputColumn::new("comment", PgType::Text),
            ],
        }
    }

    /// Whether the function returns a bare scalar rather than a composite row.
    /// PG names a scalar function's single output column after the FROM-item
    /// alias when one is given (`generate_series(1, 10) i` yields column `i`);
    /// a composite-returning function takes its column names from its row type,
    /// and a bare alias there names only the relation.
    pub fn returns_scalar(self) -> bool {
        match self {
            // All three return a record.
            TableFn::PgInputErrorInfo
            | TableFn::PgAvailableExtensions
            | TableFn::PgAvailableExtensionVersions => false,
            TableFn::GenerateSeries(_)
            | TableFn::JsonbPathQuery
            | TableFn::Unnest(_)
            | TableFn::GenerateSubscripts
            | TableFn::PgPartitionAncestors => true,
        }
    }
}

/// Resolve a set-returning function by (already lowercased) name.
pub fn lookup_table_fn(name: &str) -> Option<TableFn> {
    match name {
        "pg_input_error_info" => Some(TableFn::PgInputErrorInfo),
        "pg_partition_ancestors" => Some(TableFn::PgPartitionAncestors),
        "pg_available_extensions" => Some(TableFn::PgAvailableExtensions),
        "pg_available_extension_versions" => Some(TableFn::PgAvailableExtensionVersions),
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
    /// `array_agg(value) -> value[]`: collects the group's inputs into a
    /// one-dimensional array. Alone among the aggregates it keeps NULL inputs,
    /// as NULL elements — while an empty group is still NULL and not `{}`.
    ArrayAgg,
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
            AggFn::ArrayAgg => "array_agg",
        }
    }

    /// Whether a row whose first argument is NULL is dropped before the
    /// aggregate sees it — PostgreSQL's `strict` flag, which every aggregate
    /// here carries but `array_agg`, whose whole job is to preserve the group's
    /// values *including* its NULLs.
    ///
    /// Asked by the executor's `feed`, so the grouped and windowed drivers
    /// cannot drift on it.
    pub fn skips_null_input(self) -> bool {
        match self {
            AggFn::Count | AggFn::Min | AggFn::Max | AggFn::Sum | AggFn::Avg | AggFn::StringAgg => {
                true
            }
            AggFn::ArrayAgg => false,
        }
    }
}

/// A dedicated window function — one that has no aggregate counterpart and is
/// legal only with an `OVER` clause. Ordinary aggregates used as window
/// functions (`sum(x) OVER (…)`) are *not* here; they keep their [`AggFn`] and
/// reuse the same accumulators (see [`crate::WindowKind`]).
///
/// Every function in this set reads the current row's position within its
/// partition — its peer group, or its ordinal — and so ignores the window
/// frame entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFn {
    /// `row_number() -> int8`: the row's 1-based ordinal in the partition.
    RowNumber,
    /// `rank() -> int8`: 1 + the number of rows before this row's peer group,
    /// so ranks skip after a tie.
    Rank,
    /// `dense_rank() -> int8`: the 1-based ordinal of this row's peer group,
    /// so ranks do not skip after a tie.
    DenseRank,
}

impl WindowFn {
    /// The function's SQL name, as it appears in error messages.
    pub fn name(self) -> &'static str {
        match self {
            WindowFn::RowNumber => "row_number",
            WindowFn::Rank => "rank",
            WindowFn::DenseRank => "dense_rank",
        }
    }

    /// The function's result type. All three count rows, so all three are
    /// `int8`, as in PG.
    pub fn return_type(self) -> PgType {
        match self {
            WindowFn::RowNumber | WindowFn::Rank | WindowFn::DenseRank => PgType::Int8,
        }
    }
}

/// Resolve a dedicated window function by (already lowercased) name.
pub fn lookup_window_fn(name: &str) -> Option<WindowFn> {
    match name {
        "row_number" => Some(WindowFn::RowNumber),
        "rank" => Some(WindowFn::Rank),
        "dense_rank" => Some(WindowFn::DenseRank),
        _ => None,
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
        "array_agg" => Some(AggFn::ArrayAgg),
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
        // `array_oid_for_elem` knows only the *built-in* element↔array pairs, so
        // two arguments PostgreSQL accepts land in the same `0A000` the
        // `ARRAY[…]` constructor gives: a user enum, since `CREATE TYPE` here
        // assigns no array type of its own — the common casualty, an ordinary
        // `array_agg(mood_col)` — and an array, which PostgreSQL would stack
        // into the two-dimensional result `crabgresql_types::array` cannot hold.
        AggFn::ArrayAgg => match crabgresql_types::array::array_oid_for_elem(input_ty.oid()) {
            Some(_) => Ok(PgType::Array(input_ty.oid())),
            None => Err(BindError::feature_not_supported(format!(
                "could not find array type for data type {}",
                crate::expr::type_label(input_ty, scope.catalog().as_ref())
            ))),
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
    if name == "jsonb_path_query" {
        return Ok((
            TableFn::JsonbPathQuery,
            resolve_jsonb_path_query(&bindings)?,
        ));
    }
    if name == "unnest" {
        let (elem, args) = resolve_unnest(&bindings)?;
        return Ok((TableFn::Unnest(elem), args));
    }
    if name == "generate_subscripts" {
        return Ok((
            TableFn::GenerateSubscripts,
            resolve_generate_subscripts(&bindings)?,
        ));
    }
    if name == "pg_partition_ancestors" {
        return Ok((
            TableFn::PgPartitionAncestors,
            resolve_partition_ancestors(&bindings)?,
        ));
    }
    let Some(func) = lookup_table_fn(name) else {
        return Err(undefined_function(name, &bindings));
    };
    let params = func.arg_types();
    if params.len() != bindings.len() {
        return Err(undefined_function(name, &bindings));
    }
    // Exact-type first, then a coercing pass — same policy as scalar overloads.
    match single_candidate_args(&bindings, params) {
        Ok(args) => Ok((func, args)),
        Err(None) => Err(undefined_function(name, &bindings)),
        Err(Some(e)) => Err(e),
    }
}

/// Resolve `pg_partition_ancestors(regclass)`'s single argument, accepting
/// either spelling of a relation reference.
///
/// PostgreSQL declares one parameter, `regclass`, and takes an `oid` argument
/// anyway because the two types are binary-coercible there. Here they are not:
/// `oid` → `regclass` needs the catalog to resolve the *name* the value renders
/// as, so no pure cast can do it (`crabgresql_types::cast` has `Reg` → `Oid` and
/// deliberately not the reverse). Both are accepted by trying two signatures.
///
/// Order matters. `regclass` first is what keeps an unadorned literal resolving
/// **by name** — `pg_partition_ancestors('parts')` is the relation `parts`, as
/// in PostgreSQL. Under an `oid` parameter that literal would go to
/// `text_to_oid` and fail on the first non-digit.
///
/// Shared by FROM-position and target-list binding: psql writes this function
/// both ways in `\d`'s footers.
pub(crate) fn resolve_partition_ancestors(
    bindings: &[Binding],
) -> Result<Vec<BoundExpr>, BindError> {
    for param in [PgType::Reg(RegKind::Class), PgType::Oid] {
        if let Ok(args) = single_candidate_args(bindings, &[param]) {
            return Ok(args);
        }
    }
    Err(undefined_function("pg_partition_ancestors", bindings))
}

/// Resolve `unnest(array)` to its element type and single (array) argument.
/// Shared by FROM-position and target-list binding.
///
/// TODO: resolve `unnest` over more than one array (PG's `unnest(a, b, …)`
/// FROM-position form); only a single array argument binds here, and anything
/// else is `42883`.
pub(crate) fn resolve_unnest(bindings: &[Binding]) -> Result<(PgType, Vec<BoundExpr>), BindError> {
    if let [Binding::Typed(e)] = bindings {
        // `oidvector`/`int2vector` unnest to their element type as well, even
        // though they are not array types here — PG gives them `typelem`.
        let elem = match e.ty() {
            PgType::Array(elem_oid) => PgType::from_oid(elem_oid),
            PgType::Vector(kind) => Some(kind.element()),
            _ => None,
        };
        if let Some(elem) = elem {
            return Ok((elem, vec![e.clone()]));
        }
    }
    Err(undefined_function("unnest", bindings))
}

/// Resolve `generate_subscripts(array, dim [, reverse])` to its coerced
/// arguments. Shared by FROM-position and target-list binding.
///
/// The array parameter is `anyarray` in PG, so it is checked structurally here
/// (as [`resolve_unnest`] does) rather than through the fixed-signature table;
/// the remaining arguments are ordinary `int4`/`bool` parameters. Note the
/// deviation `unnest` already has: an *unknown* first argument is `42883` here
/// where PG reports `42804 could not determine polymorphic type`.
pub(crate) fn resolve_generate_subscripts(
    bindings: &[Binding],
) -> Result<Vec<BoundExpr>, BindError> {
    let fail = || undefined_function("generate_subscripts", bindings);
    let params: &[PgType] = match bindings.len() {
        2 => &[PgType::Int4],
        3 => &[PgType::Int4, PgType::Bool],
        _ => return Err(fail()),
    };
    let Some(Binding::Typed(array)) = bindings.first() else {
        return Err(fail());
    };
    // `oidvector`/`int2vector` carry a `typelem` in PG too, so they are
    // subscriptable — with a lower bound of 0, which the executor applies.
    if !matches!(array.ty(), PgType::Array(_) | PgType::Vector(_)) {
        return Err(fail());
    }
    // The array argument has already pinned the only candidate signature, so a
    // literal the `int4`/`bool` input function rejects is reported as PG reports
    // it (`22P02`) rather than hidden behind a `42883`. `dim` accepts int2 by
    // implicit widening but not int8, like PG's overload rules.
    match single_candidate_args(&bindings[1..], params) {
        Ok(rest) => {
            let mut args = vec![array.clone()];
            args.extend(rest);
            Ok(args)
        }
        Err(None) => Err(fail()),
        Err(Some(e)) => Err(e),
    }
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
            if let Ok(args) = try_coerce_args(bindings, &params, exact_only) {
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
                if let Ok(args) = try_coerce_args(bindings, &params, exact_only) {
                    return Ok((elem, args));
                }
            }
        }
    }
    Err(undefined_function("generate_series", bindings))
}

const F4: PgType = PgType::Float4;
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
const PATH: PgType = PgType::Path;
const BOX: PgType = PgType::Box;
const LINE: PgType = PgType::Line;
const CIRCLE: PgType = PgType::Circle;
const POLYGON: PgType = PgType::Polygon;
const JSONB: PgType = PgType::Jsonb;
const JSONPATH: PgType = PgType::Jsonpath;
const TSVECTOR: PgType = PgType::Tsvector;
const TSQUERY: PgType = PgType::Tsquery;
const TEXTARR: PgType = PgType::Array(crabgresql_types::oid::TEXT);
const OID: PgType = PgType::Oid;
const TID: PgType = PgType::Tid;
const XID: PgType = PgType::Xid;
const XID8: PgType = PgType::Xid8;
const PGLSN: PgType = PgType::PgLsn;
const REGCLASS: PgType = PgType::Reg(RegKind::Class);
const NAME: PgType = PgType::Name;
const NAMEARR: PgType = PgType::Array(crabgresql_types::oid::NAME);
const UUID: PgType = PgType::Uuid;
const I2: PgType = PgType::Int2;

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
                Signature {
                    func: ScalarFn::JsonPath($f),
                    args: &[JSONB, JSONPATH],
                    ret: $ret,
                },
                Signature {
                    func: ScalarFn::JsonPath($f),
                    args: &[JSONB, JSONPATH, JSONB],
                    ret: $ret,
                },
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
        "booleq" => &[Signature {
            func: ScalarFn::BoolEq,
            args: &[BOOL, BOOL],
            ret: BOOL,
        }],
        "boolne" => &[Signature {
            func: ScalarFn::BoolNe,
            args: &[BOOL, BOOL],
            ret: BOOL,
        }],
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
        // PG has one `abs` per numeric type. Without the narrow entries an
        // int2/int4/int8/float4 argument would land on float8, the category's
        // preferred type; the exact match is what keeps it off that path.
        "abs" => &[
            Signature {
                func: ScalarFn::AbsExact,
                args: &[PgType::Int2],
                ret: PgType::Int2,
            },
            Signature {
                func: ScalarFn::AbsExact,
                args: &[I4],
                ret: I4,
            },
            Signature {
                func: ScalarFn::AbsExact,
                args: &[PgType::Int8],
                ret: PgType::Int8,
            },
            Signature {
                func: ScalarFn::AbsExact,
                args: &[PgType::Float4],
                ret: PgType::Float4,
            },
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
        // gcd/lcm have no smallint overload in PG, and — unlike `abs` below —
        // none of the three they do have is the numeric category's preferred
        // type, so a smallint argument reaches all three and separates none:
        // `gcd(6::int2, 4::int2)` is `42725`, not a widening to int4.
        "gcd" => &[
            Signature {
                func: ScalarFn::GcdInt,
                args: &[I4, I4],
                ret: I4,
            },
            Signature {
                func: ScalarFn::GcdInt,
                args: &[PgType::Int8, PgType::Int8],
                ret: PgType::Int8,
            },
            Signature {
                func: ScalarFn::NumGcd,
                args: &[NUM, NUM],
                ret: NUM,
            },
        ],
        "lcm" => &[
            Signature {
                func: ScalarFn::LcmInt,
                args: &[I4, I4],
                ret: I4,
            },
            Signature {
                func: ScalarFn::LcmInt,
                args: &[PgType::Int8, PgType::Int8],
                ret: PgType::Int8,
            },
            Signature {
                func: ScalarFn::NumLcm,
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
        "sin" => unary_f8!(ScalarFn::Sin),
        "cos" => unary_f8!(ScalarFn::Cos),
        "tan" => unary_f8!(ScalarFn::Tan),
        "cot" => unary_f8!(ScalarFn::Cot),
        "asin" => unary_f8!(ScalarFn::Asin),
        "acos" => unary_f8!(ScalarFn::Acos),
        "atan" => unary_f8!(ScalarFn::Atan),
        "atan2" => &[Signature {
            func: ScalarFn::Atan2,
            args: &[F8, F8],
            ret: F8,
        }],
        "degrees" => unary_f8!(ScalarFn::Degrees),
        "radians" => unary_f8!(ScalarFn::Radians),
        "pi" => &[Signature {
            func: ScalarFn::Pi,
            args: &[],
            ret: F8,
        }],
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
            // Truncation in an explicit zone. Only `timestamptz` has this form
            // in PG; a `timestamp` argument reaches it through the implicit
            // `timestamp -> timestamptz` cast, and there is no `interval` one.
            Signature {
                func: ScalarFn::DateTruncTz,
                args: &[TEXT, TSTZ, TEXT],
                ret: TSTZ,
            },
        ],
        // The `timestamptz` form is listed first deliberately. Both overloads
        // sit in the datetime category, so an untyped literal reaches the
        // preferred type (`timestamptz`) through `narrow_by_unknown_category`;
        // but two `date` arguments are already typed, and the coercible pass
        // then breaks the exact-count tie by list order. PG resolves that case
        // to `timestamptz` too, so `timestamptz` has to come first for it to
        // agree. A genuine `timestamp` pair still matches exactly and never
        // reaches the tie-break.
        "date_bin" => &[
            Signature {
                func: ScalarFn::DateBinTz,
                args: &[IV, TSTZ, TSTZ],
                ret: TSTZ,
            },
            Signature {
                func: ScalarFn::DateBin,
                args: &[IV, TS, TS],
                ret: TS,
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
        // The one-argument forms are PG's SQL wrappers around
        // `age(current_date::…, $1)`. Order is load-bearing for the same reason
        // it is in `to_char` and `date_bin` below: two already-typed `date`
        // arguments only reach `resolve_call`'s best-coercible pass, where an
        // equal-cost tie is broken by list order, and PG picks the
        // `timestamptz` overload by the preferred-type rule. TSTZ leads TS at
        // each arity.
        "age" => &[
            Signature {
                func: ScalarFn::AgeTz,
                args: &[TSTZ, TSTZ],
                ret: IV,
            },
            Signature {
                func: ScalarFn::Age,
                args: &[TS, TS],
                ret: IV,
            },
            Signature {
                func: ScalarFn::AgeTodayTz,
                args: &[TSTZ],
                ret: IV,
            },
            Signature {
                func: ScalarFn::AgeToday,
                args: &[TS],
                ret: IV,
            },
            // PG's one-argument `age` spans two type categories — datetime for
            // the two above, user-defined for this one — and that is what makes
            // `age('2001-01-01')`, `age(NULL)` and `age($1)` report
            // `function age(unknown) is not unique` instead of quietly picking a
            // datetime overload. Listed last because nothing but an `xid` can
            // reach it: `coerce_for_arg` finds no implicit cast into `xid` for
            // any type, so it is dropped before any tie-break rather than by
            // its position.
            Signature {
                func: ScalarFn::AgeXid,
                args: &[XID],
                ret: I4,
            },
        ],
        // PG has no `to_char(date)` and no `to_char(time)` overload: a `date`
        // reaches the timestamptz form through the preferred-type rule
        // (`to_char(date, 'TZ')` is `UTC`, not the empty string), and a `time`
        // reaches the interval form through pg_cast's implicit
        // `time -> interval`. We spell the `time` case as its own signature
        // rather than adding that cast, which would also reshuffle operator
        // resolution for `time`. Order is load-bearing for `resolve_call`'s
        // best-coercible pass: TSTZ must lead TS so a `date` widens the way PG
        // widens it, and I4 must lead the rest so an `int2` lands on int4.
        "to_char" => &[
            Signature {
                func: ScalarFn::ToCharTimestampTz,
                args: &[TSTZ, TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToCharTimestamp,
                args: &[TS, TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToCharInterval,
                args: &[IV, TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToCharTime,
                args: &[TIME, TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToCharInt4,
                args: &[I4, TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToCharInt8,
                args: &[I8, TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToCharNumeric,
                args: &[NUM, TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToCharFloat8,
                args: &[F8, TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ToCharFloat4,
                args: &[F4, TEXT],
                ret: TEXT,
            },
        ],
        "to_date" => &[Signature {
            func: ScalarFn::ToDate,
            args: &[TEXT, TEXT],
            ret: DATE,
        }],
        // The two forms differ in arity, so they never compete.
        "to_timestamp" => &[
            Signature {
                func: ScalarFn::ToTimestampFormat,
                args: &[TEXT, TEXT],
                ret: TSTZ,
            },
            Signature {
                func: ScalarFn::ToTimestampUnix,
                args: &[F8],
                ret: TSTZ,
            },
        ],
        "to_number" => &[Signature {
            func: ScalarFn::ToNumber,
            args: &[TEXT, TEXT],
            ret: NUM,
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
        // `current_setting(name)` errors on an unknown GUC; the two-argument form
        // returns NULL instead when `missing_ok` is true.
        "current_setting" => &[
            Signature {
                func: ScalarFn::CurrentSetting,
                args: &[TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::CurrentSetting,
                args: &[TEXT, BOOL],
                ret: TEXT,
            },
        ],
        // The function form of `AT TIME ZONE`: `timezone(zone, value)`, with the
        // zone as either a name or a fixed `interval` displacement. The one-arg
        // overloads are the function form of `AT LOCAL` — the zone is the
        // session's, read at execution time.
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
            Signature {
                func: ScalarFn::TimezoneTimeTz,
                args: &[TEXT, TIMETZ],
                ret: TIMETZ,
            },
            Signature {
                func: ScalarFn::TimezoneIntervalToTz,
                args: &[IV, TS],
                ret: TSTZ,
            },
            Signature {
                func: ScalarFn::TimezoneIntervalToTs,
                args: &[IV, TSTZ],
                ret: TS,
            },
            Signature {
                func: ScalarFn::TimezoneIntervalTimeTz,
                args: &[IV, TIMETZ],
                ret: TIMETZ,
            },
            Signature {
                func: ScalarFn::TimezoneLocalToTz,
                args: &[TS],
                ret: TSTZ,
            },
            Signature {
                func: ScalarFn::TimezoneLocalToTs,
                args: &[TSTZ],
                ret: TS,
            },
            Signature {
                func: ScalarFn::TimezoneLocalTimeTz,
                args: &[TIMETZ],
                ret: TIMETZ,
            },
        ],
        // Two overloads. A bare `md5('abc')` answers from the text one: an
        // unknown literal prefers a `String`-category candidate over bytea's
        // `UserDefined` (`narrow_by_unknown_category`), so the order the two
        // appear in is not what decides it. A typed `bytea` argument never
        // coerces to text (see `implicit_castable`), so `md5(x::bytea)` binds
        // the bytea one.
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
        // The SHA-2 and CRC families take bytea only — there is no text
        // overload, so `sha256('abc'::text)` is a 42883 while the unknown
        // literal in `sha256('abc')` resolves through byteain. Their result is
        // bytea (rendered `\x…`), not md5's hex text.
        "sha224" => &[Signature {
            func: ScalarFn::Sha224,
            args: &[BYTEA],
            ret: BYTEA,
        }],
        "sha256" => &[Signature {
            func: ScalarFn::Sha256,
            args: &[BYTEA],
            ret: BYTEA,
        }],
        "sha384" => &[Signature {
            func: ScalarFn::Sha384,
            args: &[BYTEA],
            ret: BYTEA,
        }],
        "sha512" => &[Signature {
            func: ScalarFn::Sha512,
            args: &[BYTEA],
            ret: BYTEA,
        }],
        // int8, not int4: the checksum is unsigned 32-bit, so 4213642571 has
        // to stay positive.
        "crc32" => &[Signature {
            func: ScalarFn::Crc32,
            args: &[BYTEA],
            ret: I8,
        }],
        "crc32c" => &[Signature {
            func: ScalarFn::Crc32c,
            args: &[BYTEA],
            ret: I8,
        }],
        // The uuid readers. Unlike the generators above these are pure — they
        // answer from their argument alone — so `eval_scalar` holds them.
        "uuid_extract_version" => &[Signature {
            func: ScalarFn::UuidExtractVersion,
            args: &[UUID],
            ret: I2,
        }],
        "uuid_extract_timestamp" => &[Signature {
            func: ScalarFn::UuidExtractTimestamp,
            args: &[UUID],
            ret: TSTZ,
        }],
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
            // `length(bytea)` counts bytes, not characters. A bare
            // `length('abc')` still answers 3: bytea's category is
            // `UserDefined`, and an unknown literal prefers a `String`
            // candidate (`narrow_by_unknown_category`), so the order these
            // two signatures appear in is not what decides it.
            Signature {
                func: ScalarFn::Length,
                args: &[BYTEA],
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
            // `length(path)` is the total segment length, not a count.
            Signature {
                func: ScalarFn::Geo(GeoFn::PathLength),
                args: &[PATH],
                ret: F8,
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
            Signature {
                func: ScalarFn::OctetLength,
                args: &[BYTEA],
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
            // `bit_length(bytea)` is eight times the byte count.
            Signature {
                func: ScalarFn::BitLength,
                args: &[BYTEA],
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
        // Sequence functions. PG declares these over `regclass` only, so a bare
        // `nextval('seq')` types its unknown literal from that — which is what
        // makes `nextval('S1')` and `nextval(' s1 ')` find `s1`, since the
        // `regclass` input normalizes an unquoted name the way the parser would.
        // These are side-effecting and are dispatched by the executor's `eval`
        // (not `eval_scalar`).
        "nextval" => &[Signature {
            func: ScalarFn::Nextval,
            args: &[REGCLASS],
            ret: I8,
        }],
        "currval" => &[Signature {
            func: ScalarFn::Currval,
            args: &[REGCLASS],
            ret: I8,
        }],
        "setval" => &[
            Signature {
                func: ScalarFn::Setval,
                args: &[REGCLASS, I8],
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
        // The clock. Zero-arity and impure, so the executor's `eval` dispatches
        // them from the session's stamped instants; nothing folds them here,
        // since the binder does not fold calls at all.
        //
        // The `CURRENT_TIMESTAMP` family is deliberately absent: those are
        // keywords the grammar rewrites, not `pg_proc` entries, so a quoted
        // `"current_timestamp"()` must still be an unknown function — as in PG.
        "now" | "transaction_timestamp" => &[Signature {
            func: ScalarFn::TransactionTimestamp,
            args: &[],
            ret: TSTZ,
        }],
        "statement_timestamp" => &[Signature {
            func: ScalarFn::StatementTimestamp,
            args: &[],
            ret: TSTZ,
        }],
        "clock_timestamp" => &[Signature {
            func: ScalarFn::ClockTimestamp,
            args: &[],
            ret: TSTZ,
        }],
        // UUID generation. Zero-arity (bar the shift) and impure, so `eval`
        // dispatches these from the clock and the RNG, next to the family above.
        //
        // `uuidv4` is PG's RFC-9562 spelling of `gen_random_uuid`, sharing one
        // implementation upstream; one `ScalarFn` for both says the same thing.
        "gen_random_uuid" | "uuidv4" => &[Signature {
            func: ScalarFn::GenRandomUuid,
            args: &[],
            ret: UUID,
        }],
        "uuidv7" => &[
            Signature {
                func: ScalarFn::UuidV7,
                args: &[],
                ret: UUID,
            },
            Signature {
                func: ScalarFn::UuidV7Shift,
                args: &[IV],
                ret: UUID,
            },
        ],
        // Not a clock reading but an instant like one: fixed for the life of the
        // process, so `eval` answers it alongside the family above.
        "pg_postmaster_start_time" => &[Signature {
            func: ScalarFn::PgPostmasterStartTime,
            args: &[],
            ret: TSTZ,
        }],
        // The server's build identity. Zero-arity and constant, so unlike the
        // rest of this neighborhood it needs nothing from the session.
        "version" => &[Signature {
            func: ScalarFn::Version,
            args: &[],
            ret: TEXT,
        }],
        // The connection identity. `current_catalog` is PostgreSQL grammar for
        // the same function — there is no `pg_proc` row for it — so both keys
        // land on one signature, and `current_catalog()` is a syntax error in
        // both systems because the parser never produces a call for it.
        "current_database" | "current_catalog" => &[Signature {
            func: ScalarFn::CurrentDatabase,
            args: &[],
            ret: NAME,
        }],
        // PostgreSQL spells this both ways: `current_schema` is a keyword *and*
        // a real `pg_proc` entry, so `current_schema()` is legal — unlike
        // `current_user()`.
        "current_schema" => &[Signature {
            func: ScalarFn::CurrentSchema,
            args: &[],
            ret: NAME,
        }],
        "current_schemas" => &[Signature {
            func: ScalarFn::CurrentSchemas,
            args: &[BOOL],
            ret: NAMEARR,
        }],
        // `current_role` and `user` have no `pg_proc` row of their own either.
        "current_user" | "current_role" | "user" => &[Signature {
            func: ScalarFn::CurrentUser,
            args: &[],
            ret: NAME,
        }],
        "session_user" => &[Signature {
            func: ScalarFn::SessionUser,
            args: &[],
            ret: NAME,
        }],
        "pg_my_temp_schema" => &[Signature {
            func: ScalarFn::PgMyTempSchema,
            args: &[],
            ret: OID,
        }],
        "pg_backend_pid" => &[Signature {
            func: ScalarFn::PgBackendPid,
            args: &[],
            ret: I4,
        }],
        "pg_is_other_temp_schema" => &[Signature {
            func: ScalarFn::PgIsOtherTempSchema,
            args: &[OID],
            ret: BOOL,
        }],
        // PG renamed `txid_*` to `pg_*_xact_id` in v13 and kept both. The old
        // pair reports the same number as `int8` because it predates `xid8`.
        "txid_current" => &[Signature {
            func: ScalarFn::CurrentXactId {
                xid8: false,
                if_assigned: false,
            },
            args: &[],
            ret: I8,
        }],
        "txid_current_if_assigned" => &[Signature {
            func: ScalarFn::CurrentXactId {
                xid8: false,
                if_assigned: true,
            },
            args: &[],
            ret: I8,
        }],
        "pg_current_xact_id" => &[Signature {
            func: ScalarFn::CurrentXactId {
                xid8: true,
                if_assigned: false,
            },
            args: &[],
            ret: XID8,
        }],
        "pg_current_xact_id_if_assigned" => &[Signature {
            func: ScalarFn::CurrentXactId {
                xid8: true,
                if_assigned: true,
            },
            args: &[],
            ret: XID8,
        }],
        "pg_xact_status" => &[Signature {
            func: ScalarFn::PgXactStatus,
            args: &[XID8],
            ret: TEXT,
        }],
        "pg_is_in_recovery" => &[Signature {
            func: ScalarFn::PgIsInRecovery,
            args: &[],
            ret: BOOL,
        }],
        "pg_encoding_to_char" => &[Signature {
            func: ScalarFn::PgEncodingToChar,
            args: &[I4],
            ret: NAME,
        }],
        "pg_char_to_encoding" => &[Signature {
            func: ScalarFn::PgCharToEncoding,
            args: &[NAME],
            ret: I4,
        }],
        // The access-method property functions. `pg_indexam_has_property` needs
        // nothing but its OID and is answered by the pure `eval_scalar`; the two
        // that take an index read the catalog, so `eval` dispatches them — and
        // they are absent from this table entirely, resolving their own arguments
        // in `resolve_index_property` because their index parameter is spelled
        // either `regclass` or `oid`.
        "pg_indexam_has_property" => &[Signature {
            func: ScalarFn::PgIndexamHasProperty,
            args: &[OID, TEXT],
            ret: BOOL,
        }],
        "pg_tablespace_location" => &[Signature {
            func: ScalarFn::PgTablespaceLocation,
            args: &[OID],
            ret: TEXT,
        }],
        // The four *size* functions resolve in `resolve_relation_size` (their
        // relation parameter is spelled either way), but this one takes a plain
        // number and belongs in the table. Two overloads with nothing to choose
        // between them for an integer argument is the point: PostgreSQL answers
        // `pg_size_pretty(1024)` with `42725 is not unique`, which the tie rule
        // in `resolve_call` reproduces.
        "pg_size_pretty" => &[
            Signature {
                func: ScalarFn::PgSizePretty,
                args: &[I8],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgSizePretty,
                args: &[NUM],
                ret: TEXT,
            },
        ],
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
        "obj_description" => &[
            Signature {
                func: ScalarFn::ObjDescription,
                args: &[OID],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::ObjDescription,
                args: &[OID, NAME],
                ret: TEXT,
            },
        ],
        "col_description" => &[Signature {
            func: ScalarFn::ColDescription,
            args: &[OID, I4],
            ret: TEXT,
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
        // Five signatures, as PostgreSQL declares five functions. They coexist in
        // the table rather than behind an ordered resolver — the way
        // `pg_relation_size`'s two spellings do — because a bare
        // `pg_get_viewdef('v')` is not ambiguous between them: the string
        // category rule in `narrow_by_unknown_category` sends an unknown literal
        // to `text`, which is exactly how PostgreSQL resolves the same call.
        "pg_get_viewdef" => &[
            Signature {
                func: ScalarFn::PgGetViewdef,
                args: &[TEXT],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetViewdef,
                args: &[TEXT, BOOL],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetViewdef,
                args: &[OID],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetViewdef,
                args: &[OID, BOOL],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetViewdef,
                args: &[OID, I4],
                ret: TEXT,
            },
        ],
        "pg_get_ruledef" => &[
            Signature {
                func: ScalarFn::PgGetRuledef,
                args: &[OID],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetRuledef,
                args: &[OID, BOOL],
                ret: TEXT,
            },
        ],
        "pg_get_triggerdef" => &[
            Signature {
                func: ScalarFn::PgGetTriggerdef,
                args: &[OID],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetTriggerdef,
                args: &[OID, BOOL],
                ret: TEXT,
            },
        ],
        "pg_get_function_arguments" => &[Signature {
            func: ScalarFn::PgGetFunctionArguments,
            args: &[OID],
            ret: TEXT,
        }],
        "pg_get_function_identity_arguments" => &[Signature {
            func: ScalarFn::PgGetFunctionIdentityArguments,
            args: &[OID],
            ret: TEXT,
        }],
        "pg_get_function_result" => &[Signature {
            func: ScalarFn::PgGetFunctionResult,
            args: &[OID],
            ret: TEXT,
        }],
        "pg_get_indexdef" => &[
            Signature {
                func: ScalarFn::PgGetIndexdef,
                args: &[OID],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetIndexdef,
                args: &[OID, I4, BOOL],
                ret: TEXT,
            },
        ],
        "pg_get_partkeydef" => &[Signature {
            func: ScalarFn::PgGetPartkeydef,
            args: &[OID],
            ret: TEXT,
        }],
        "pg_get_serial_sequence" => &[Signature {
            func: ScalarFn::PgGetSerialSequence,
            args: &[TEXT, TEXT],
            ret: TEXT,
        }],
        "pg_get_constraintdef" => &[
            Signature {
                func: ScalarFn::PgGetConstraintdef,
                args: &[OID],
                ret: TEXT,
            },
            Signature {
                func: ScalarFn::PgGetConstraintdef,
                args: &[OID, BOOL],
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
        // output differs (`10.1/16` vs `10.1.0.0/16`); an untyped literal still
        // resolves to inet, the preferred type of the inet/cidr category, while
        // a typed cidr binds the cidr overload by exact match.
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
        "point" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::PointConstruct),
                args: &[F8, F8],
                ret: POINT,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::BoxCenter),
                args: &[BOX],
                ret: POINT,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::CircleCenter),
                args: &[CIRCLE],
                ret: POINT,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::PolyCenter),
                args: &[POLYGON],
                ret: POINT,
            },
        ],
        "lseg" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::LsegConstruct),
                args: &[POINT, POINT],
                ret: LSEG,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::BoxDiagonal),
                args: &[BOX],
                ret: LSEG,
            },
        ],
        "box" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::BoxFromPoint),
                args: &[POINT],
                ret: BOX,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::BoxConstruct),
                args: &[POINT, POINT],
                ret: BOX,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::CircleToBox),
                args: &[CIRCLE],
                ret: BOX,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::PolyToBox),
                args: &[POLYGON],
                ret: BOX,
            },
        ],
        "line" => &[Signature {
            func: ScalarFn::Geo(GeoFn::LineConstruct),
            args: &[POINT, POINT],
            ret: LINE,
        }],
        "circle" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::CircleConstruct),
                args: &[POINT, F8],
                ret: CIRCLE,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::BoxToCircle),
                args: &[BOX],
                ret: CIRCLE,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::CircleFromPolygon),
                args: &[POLYGON],
                ret: CIRCLE,
            },
        ],
        "polygon" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::BoxToPolygon),
                args: &[BOX],
                ret: POLYGON,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::CircleToPolygon),
                args: &[CIRCLE],
                ret: POLYGON,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::CircleToPolygonN),
                args: &[I4, CIRCLE],
                ret: POLYGON,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::PathToPolygon),
                args: &[PATH],
                ret: POLYGON,
            },
        ],
        "path" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PolyToPath),
            args: &[POLYGON],
            ret: PATH,
        }],
        "center" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::BoxCenter),
                args: &[BOX],
                ret: POINT,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::CircleCenter),
                args: &[CIRCLE],
                ret: POINT,
            },
        ],
        "diagonal" => &[Signature {
            func: ScalarFn::Geo(GeoFn::BoxDiagonal),
            args: &[BOX],
            ret: LSEG,
        }],
        "width" => &[Signature {
            func: ScalarFn::Geo(GeoFn::BoxWidth),
            args: &[BOX],
            ret: F8,
        }],
        "height" => &[Signature {
            func: ScalarFn::Geo(GeoFn::BoxHeight),
            args: &[BOX],
            ret: F8,
        }],
        "bound_box" => &[Signature {
            func: ScalarFn::Geo(GeoFn::BoundBox),
            args: &[BOX, BOX],
            ret: BOX,
        }],
        "radius" => &[Signature {
            func: ScalarFn::Geo(GeoFn::CircleRadius),
            args: &[CIRCLE],
            ret: F8,
        }],
        "diameter" => &[Signature {
            func: ScalarFn::Geo(GeoFn::CircleDiameter),
            args: &[CIRCLE],
            ret: F8,
        }],
        "isparallel" => &[Signature {
            func: ScalarFn::Geo(GeoFn::LineParallel),
            args: &[LINE, LINE],
            ret: BOOL,
        }],
        "isperp" => &[Signature {
            func: ScalarFn::Geo(GeoFn::LinePerpendicular),
            args: &[LINE, LINE],
            ret: BOOL,
        }],
        "pt_contained_circle" => &[Signature {
            func: ScalarFn::Geo(GeoFn::CircleContainPtSwapped),
            args: &[POINT, CIRCLE],
            ret: BOOL,
        }],
        "pt_contained_poly" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PolyContainPtSwapped),
            args: &[POINT, POLYGON],
            ret: BOOL,
        }],
        "slope" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PointSlope),
            args: &[POINT, POINT],
            ret: F8,
        }],
        "ishorizontal" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::PointHoriz),
                args: &[POINT, POINT],
                ret: BOOL,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::LineHoriz),
                args: &[LINE],
                ret: BOOL,
            },
        ],
        "isvertical" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::PointVert),
                args: &[POINT, POINT],
                ret: BOOL,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::LineVert),
                args: &[LINE],
                ret: BOOL,
            },
        ],
        "isopen" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PathIsOpen),
            args: &[PATH],
            ret: BOOL,
        }],
        "isclosed" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PathIsClosed),
            args: &[PATH],
            ret: BOOL,
        }],
        "popen" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PathPopen),
            args: &[PATH],
            ret: PATH,
        }],
        "pclose" => &[Signature {
            func: ScalarFn::Geo(GeoFn::PathPclose),
            args: &[PATH],
            ret: PATH,
        }],
        // A `BlockNumber` is unsigned 32-bit, so it needs `int8` to render
        // without wrapping; an `OffsetNumber` fits `int4`.
        "tid_block" => &[Signature {
            func: ScalarFn::TidBlock,
            args: &[TID],
            ret: I8,
        }],
        "tid_offset" => &[Signature {
            func: ScalarFn::TidOffset,
            args: &[TID],
            ret: I4,
        }],
        "pg_lsn" => &[Signature {
            func: ScalarFn::NumericPgLsn,
            args: &[NUM],
            ret: PGLSN,
        }],
        "xid8cmp" => &[Signature {
            func: ScalarFn::Xid8Cmp,
            args: &[XID8, XID8],
            ret: I4,
        }],
        "npoints" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::PathNpoints),
                args: &[PATH],
                ret: I4,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::PolyNpoints),
                args: &[POLYGON],
                ret: I4,
            },
        ],
        "area" => &[
            Signature {
                func: ScalarFn::Geo(GeoFn::PathArea),
                args: &[PATH],
                ret: F8,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::BoxArea),
                args: &[BOX],
                ret: F8,
            },
            Signature {
                func: ScalarFn::Geo(GeoFn::CircleArea),
                args: &[CIRCLE],
                ret: F8,
            },
        ],
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
///
/// The qualifier is dropped rather than checked, which is what makes
/// `pg_catalog.pg_get_expr(...)` bind — and, through
/// [`bind_table_fn_call`]'s caller, `pg_catalog.pg_partition_ancestors(...)` in
/// FROM position. Every function this build knows is a built-in living in
/// `pg_catalog`, so there is no second candidate a qualifier could pick between.
pub(crate) fn function_name(name: &ast::ObjectName) -> Option<String> {
    name.0
        .last()
        .and_then(|p| p.as_ident())
        .map(crate::expr::normalize_ident)
}

/// The shorthands PostgreSQL implements in its grammar rather than in
/// `pg_proc`. None is polymorphic enough for the overload table: `COALESCE` and
/// `GREATEST`/`LEAST` are variadic and take their result type from the one type
/// their whole argument list unifies to; `NULLIF` hands back its left argument,
/// so its result type is that argument's, as the `=` it resolves takes it —
/// none is a fixed signature.
#[derive(Clone, Copy)]
enum SpecialForm {
    Coalesce,
    NullIf,
    MinMax(MinMaxKind),
}

/// `COALESCE`/`NULLIF`/`GREATEST`/`LEAST` written the one way PG's grammar
/// admits them: as an **unqualified, unquoted** keyword.
///
/// Both other spellings are ordinary function-name lookups in PG, and no schema
/// holds a function by either name — so `pg_catalog.coalesce(1, 2)` and
/// `"coalesce"(1, 2)` are both `42883`, which is what returning `None` here (and
/// falling through to `resolve_call`) produces.
fn special_form(name: &ast::ObjectName) -> Option<SpecialForm> {
    let [part] = name.0.as_slice() else {
        return None;
    };
    let ident = part.as_ident().filter(|id| id.quote_style.is_none())?;
    match crate::expr::normalize_ident(ident).as_str() {
        "coalesce" => Some(SpecialForm::Coalesce),
        "nullif" => Some(SpecialForm::NullIf),
        "greatest" => Some(SpecialForm::MinMax(MinMaxKind::Greatest)),
        "least" => Some(SpecialForm::MinMax(MinMaxKind::Least)),
        _ => None,
    }
}

/// `ARRAY` written the one way PG's grammar admits the array-subquery
/// constructor: as an **unqualified, unquoted** keyword — the same rule
/// [`special_form`] applies to `COALESCE`/`NULLIF`, and for the same reason.
///
/// The shape, not the last part (what [`function_name`] returns): only the
/// qualifier's presence separates `pg_catalog.array(SELECT 1)` from the bare
/// form. The parser refuses to attach that qualifier at all, so this is the
/// second gate — kept exact so the binder recognizes its own grammar without
/// depending on the first.
fn array_keyword(name: &ast::ObjectName) -> bool {
    let [part] = name.0.as_slice() else {
        return false;
    };
    part.as_ident()
        .filter(|id| id.quote_style.is_none())
        .is_some_and(|id| crate::expr::normalize_ident(id) == "array")
}

/// Bind `COALESCE(…)` / `NULLIF(…)` / `GREATEST(…)` / `LEAST(…)`.
///
/// PG's grammar gives these a bare expression list and nothing else, so every
/// decoration an aggregate or window call may carry is a *syntax* error there. This
/// parser accepts them all on any call, so each is rejected here — naming the token
/// PG's cursor would sit under, and in the order its parser meets them: inside the
/// argument list first, then `FILTER`, `WITHIN GROUP`, and `OVER`.
fn bind_special_form(
    form: SpecialForm,
    func: &ast::Function,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let syntax_error = |token: &str| {
        Err(BindError::new(
            sqlstate::SYNTAX_ERROR,
            format!("syntax error at or near \"{token}\""),
        ))
    };
    if let ast::FunctionArguments::List(list) = &func.args {
        if let Some(treatment) = list.duplicate_treatment {
            return syntax_error(match treatment {
                ast::DuplicateTreatment::Distinct => "distinct",
                ast::DuplicateTreatment::All => "all",
            });
        }
        if let Some(clause) = list.clauses.first() {
            return syntax_error(argument_clause_token(clause));
        }
        // A named argument (`coalesce(x => 1)`) is reported at its operator, which
        // is where PG's cursor sits.
        for arg in &list.args {
            match arg {
                ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(_)) => {}
                ast::FunctionArg::Named { operator, .. }
                | ast::FunctionArg::ExprNamed { operator, .. } => {
                    return syntax_error(&operator.to_string());
                }
                // `VARIADIC` belongs to PG's *function* argument list, not to
                // the bare expression list these forms take, so its cursor
                // stops on the keyword itself.
                ast::FunctionArg::Variadic(_) => return syntax_error("variadic"),
                // PG's grammar has no bare wildcard in an expression list either,
                // and its cursor stops at the star — including for the Snowflake
                // `* EXCLUDE(…)` this parser accepts.
                ast::FunctionArg::Unnamed(
                    ast::FunctionArgExpr::Wildcard | ast::FunctionArgExpr::WildcardWithOptions(_),
                ) => {
                    return syntax_error("*");
                }
                // `t.*` is no syntax error in PG: it is a whole-row reference, and
                // `greatest(t.*)` over `(1,2)` hands back the `record` `(1,2)`.
                //
                // TODO: whole-row references, which need a composite/`record` type
                // in `PgType` before an expression can carry one.
                ast::FunctionArg::Unnamed(ast::FunctionArgExpr::QualifiedWildcard(_)) => {
                    return Err(BindError::feature_not_supported(
                        "whole-row references are not supported yet",
                    ));
                }
            }
        }
    }
    if func.filter.is_some() {
        return syntax_error("filter");
    }
    if !func.within_group.is_empty() {
        return syntax_error("within");
    }
    if func.over.is_some() {
        return syntax_error("over");
    }
    if let Some(treatment) = func.null_treatment {
        return syntax_error(null_treatment_token(treatment));
    }
    let bindings = positional_args(&func.args)?
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    match form {
        SpecialForm::Coalesce => crate::expr::bind_coalesce(bindings, scope),
        SpecialForm::NullIf => crate::expr::bind_nullif(bindings, scope),
        SpecialForm::MinMax(kind) => crate::expr::bind_min_max(kind, bindings, scope),
    }
}

/// The keyword PG's parser stops at for a clause written inside an argument list.
fn argument_clause_token(clause: &ast::FunctionArgumentClause) -> &'static str {
    match clause {
        ast::FunctionArgumentClause::IgnoreOrRespectNulls(treatment) => {
            null_treatment_token(*treatment)
        }
        ast::FunctionArgumentClause::OrderBy(_) => "order",
        ast::FunctionArgumentClause::Limit(_) => "limit",
        ast::FunctionArgumentClause::OnOverflow(_) => "on",
        ast::FunctionArgumentClause::Having(_) => "having",
        ast::FunctionArgumentClause::Separator(_) => "separator",
        ast::FunctionArgumentClause::JsonNullClause(_) => "null",
        ast::FunctionArgumentClause::JsonReturningClause(_) => "returning",
    }
}

fn null_treatment_token(treatment: ast::NullTreatment) -> &'static str {
    match treatment {
        ast::NullTreatment::IgnoreNulls => "ignore",
        ast::NullTreatment::RespectNulls => "respect",
    }
}

pub(crate) fn bind_function(func: &ast::Function, scope: &Scope) -> Result<Binding, BindError> {
    // `COALESCE`/`NULLIF`/`GREATEST`/`LEAST` are grammar constructs, so they are
    // recognized before anything else a *function* call can carry: every
    // decoration below is a syntax error in PG's grammar, not an unsupported
    // function form.
    if let Some(form) = special_form(&func.name) {
        return bind_special_form(form, func, scope);
    }
    if func.filter.is_some() || !func.within_group.is_empty() || func.null_treatment.is_some() {
        return Err(BindError::feature_not_supported(
            "this function form is not supported yet",
        ));
    }
    let Some(name) = function_name(&func.name) else {
        return Err(BindError::feature_not_supported(format!(
            "function is not supported yet: {func}"
        )));
    };
    // `ARRAY(SELECT …)` is grammar, not a call: the parser spells it as a
    // function whose argument list *is* a query, a shape nothing else produces
    // (a user's `array(1)` arrives as a positional list). Dispatched before the
    // window and aggregate paths because none of their decorations are
    // grammatical here.
    if array_keyword(&func.name)
        && let ast::FunctionArguments::Subquery(query) = &func.args
    {
        return crate::expr::bind_array_subquery(query, scope);
    }
    // An `OVER` clause makes this a window call whatever the name resolves to,
    // so it is dispatched before the aggregate and scalar paths.
    if let Some(over) = &func.over {
        return bind_window_call(&name, func, over, scope);
    }
    // The dedicated window functions exist only in window position — but only
    // when the call actually resolves to one. PG resolves by name and argument
    // types first and reports the missing OVER clause about the *chosen*
    // function, so `CREATE FUNCTION rank(int) …; SELECT rank(1)` calls the user's.
    if let Some(win) = lookup_window_fn(&name)
        && builtin_window_args_match(&func.args)
    {
        return Err(BindError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("window function {} requires an OVER clause", win.name()),
        ));
    }
    // The `CURRENT_TIMESTAMP` family: grammar keywords, not functions, so they
    // are rewritten here rather than resolved through the overload table.
    if let Some(binding) = bind_current_datetime(&name, &func.name, &func.args)? {
        return Ok(binding);
    }
    // Aggregates bind to a transient `Aggregate` marker (extracted into an
    // `Aggregate` plan node later), not to a scalar overload.
    if let Some(agg) = lookup_agg(&name) {
        return bind_aggregate(agg, &name, &func.args, scope);
    }
    let arg_exprs = positional_args(&func.args)?;
    let variadic = variadic_arg_index(&func.args);
    let mut bindings = arg_exprs
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;

    // `concat`/`concat_ws`/`format` are variadic and non-strict; they don't fit
    // the fixed-arity overload table, so every argument is coerced to text and a
    // single variadic `FuncCall` is built directly.
    //
    // `spread_at` is where the callee's `VARIADIC "any"` parameter sits; below
    // it the parameter is an ordinary one, which is why a `VARIADIC` argument
    // landing on `concat_ws`'s separator is `function concat_ws(text[]) does
    // not exist` rather than a spread.
    if let Some((plain, spread_fn, spread_at, min_args)) = match name.as_str() {
        "concat" => Some((ScalarFn::Concat, ScalarFn::ConcatVariadic, 0, 1)),
        "concat_ws" => Some((ScalarFn::ConcatWs, ScalarFn::ConcatWsVariadic, 1, 2)),
        // `format(text)` is a second `pg_proc` entry, not the variadic one, so
        // one argument is enough.
        "format" => Some((ScalarFn::Format, ScalarFn::FormatVariadic, 1, 1)),
        _ => None,
    } {
        // `concat()` and `concat_ws(',')` are 42883 on 18.4, not the empty
        // string: the variadic parameter needs at least one argument. Spelled
        // with the keyword the array *is* that whole parameter, so it also has
        // to land exactly on it — `concat_ws(',', 'a', VARIADIC arr)` has one
        // argument too many for `concat_ws(text, VARIADIC "any")`.
        if bindings.len() < min_args || variadic.is_some_and(|i| i != spread_at) {
            return Err(undefined_function(&name, &bindings));
        }
        if let Some(i) = variadic {
            if !matches!(&bindings[i], Binding::Typed(e) if matches!(e.ty(), PgType::Array(_))) {
                return Err(variadic_not_array(arg_exprs[i].span()));
            }
            // The array keeps its own type where the other operands are coerced
            // to text: `VARIADIC "any"` renders each *element* with that
            // element's output function.
            let mut args = bindings
                .drain(..i)
                .map(crate::expr::to_concat_operand)
                .collect::<Result<Vec<_>, _>>()?;
            let Binding::Typed(array) = bindings.remove(0) else {
                unreachable!("checked above");
            };
            args.push(array);
            return finish_func_call(spread_fn, PgType::Text, args);
        }
        let args = bindings
            .into_iter()
            .map(crate::expr::to_concat_operand)
            .collect::<Result<Vec<_>, _>>()?;
        return finish_func_call(plain, PgType::Text, args);
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

    // The two index-property functions take a relation reference in either
    // spelling, so they resolve outside the overload table too.
    if let Some(binding) = resolve_index_property(&name, &bindings)? {
        return Ok(binding);
    }

    // The size functions take one for the same reason.
    if let Some(binding) = resolve_relation_size(&name, &bindings)? {
        return Ok(binding);
    }

    // `pg_typeof(any)` accepts every type, so it has no fixed signature either.
    // Only the unary form is ours; any other arity falls through, so a
    // user-defined overload of this name stays reachable.
    if name == "pg_typeof" && bindings.len() == 1 {
        let binding = bindings.into_iter().next().expect("exactly one argument");
        return bind_pg_typeof(binding, scope);
    }

    resolve_call_variadic(&name, bindings, scope.catalog(), variadic.is_some())
}

/// Bind `pg_typeof(any) -> regtype`, which reports its argument's type.
///
/// The reported OID comes from the argument's *static* type and rides on
/// [`ScalarFn::PgTypeof`]; the argument itself stays in `args`. That matters more
/// than it looks: `pg_typeof` is an ordinary function in PG, so the argument is
/// still evaluated (`pg_typeof(1/0)` raises, `pg_typeof(nextval('s'))` advances
/// the sequence), and every pass that walks `FuncCall.args` — aggregate
/// extraction, GROUP BY validation, volatility, deparse — has to keep seeing it.
/// Collapsing the call to a bare OID constant, as an earlier version did, made
/// all of those quietly wrong.
///
/// The name is resolved at run time against the catalog rather than folded in
/// here, so a user type prints correctly and a prepared statement stays honest if
/// the type is renamed between bind and execute.
///
/// Note the reported type carries no modifier — `pg_typeof(1::numeric(10,2))`
/// is `numeric`, not `numeric(10,2)` — because a `regtype` is only an OID.
fn bind_pg_typeof(binding: Binding, scope: &Scope) -> Result<Binding, BindError> {
    let (oid, arg) = match binding {
        Binding::Typed(expr) => (expr.ty().oid(), expr),
        // A `$n` still awaiting context has no type to report, and unlike an
        // ordinary call site `pg_typeof` gives it none, so PG gives up here
        // rather than at Describe time.
        Binding::Unknown {
            param: Some((index, _)),
            ..
        } => {
            return Err(BindError::new(
                "42P18",
                format!("could not determine data type of parameter ${}", index + 1),
            ));
        }
        // A bare literal or `NULL` really is of type `unknown`; PG reports that
        // rather than resolving it to text the way most call sites would. The
        // literal is still resolved — to text, arbitrarily — only so `args` holds
        // a real expression to evaluate and to deparse; its type never escapes,
        // because `ruleutils::call_arg_types` declines to relabel a `pg_typeof`
        // argument.
        Binding::Unknown { lit, span, param } => (
            crabgresql_types::oid::UNKNOWN,
            crate::expr::resolve_unknown_ctx(scope.catalog(), lit, span, param, PgType::Text)?,
        ),
    };
    finish_func_call(
        ScalarFn::PgTypeof(oid),
        PgType::Reg(RegKind::Type),
        vec![arg],
    )
}

/// Bind a call carrying an `OVER` clause to a transient
/// [`BoundExpr::WindowFunc`] marker. The binder's window-extraction pass later
/// moves it into a [`crate::LogicalPlan::Window`] node and replaces the marker
/// with a `ColumnRef`.
///
/// The name resolves in two namespaces: the dedicated window functions, and the
/// ordinary aggregates, which `OVER` turns into window aggregates. Anything else
/// is PG's "not a window function nor an aggregate function".
fn bind_window_call(
    name: &str,
    func: &ast::Function,
    over: &ast::WindowType,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let spec = resolve_over_clause(over, scope)?;
    let spec = bind_window_spec(&spec, scope)?;
    let kind = if let Some(win) =
        lookup_window_fn(name).filter(|_| builtin_window_args_match(&func.args))
    {
        WindowKind::Builtin {
            func: win,
            args: Vec::new(),
        }
    } else if let Some(agg) = lookup_agg(name) {
        let Binding::Typed(BoundExpr::Aggregate {
            func,
            distinct,
            agg_args,
            order_by,
            input_ty,
            ret,
        }) = bind_aggregate(agg, name, &func.args, scope)?
        else {
            return Err(BindError::new(
                sqlstate::INTERNAL_ERROR,
                "aggregate bound to a non-aggregate",
            ));
        };
        // PG has never implemented this — the accumulators would need a
        // per-frame distinct set — so it is a permanent 0A000, not a gap.
        if distinct {
            return Err(BindError::feature_not_supported(
                "DISTINCT is not implemented for window functions",
            ));
        }
        // Nor this, and for the same reason: an aggregate's own ORDER BY would
        // have to re-sort each frame, which the streaming accumulators cannot.
        if !order_by.is_empty() {
            return Err(BindError::feature_not_supported(
                "aggregate ORDER BY is not implemented for window functions",
            ));
        }
        WindowKind::Aggregate(BoundAggregate {
            func,
            distinct: false,
            collation: agg_args.first().map_or(DEFAULT_COLLATION_OID, |a| {
                crate::collation::expr_collation(a).collation
            }),
            args: agg_args,
            order_by,
            input_ty,
            ret,
        })
    } else {
        // Resolve the name first, so an outright typo still reports "function
        // <name>(<types>) does not exist" rather than blaming the OVER clause.
        let arg_exprs = positional_args(&func.args)?;
        let bindings = arg_exprs
            .iter()
            .map(|e| bind_expr(e, scope))
            .collect::<Result<Vec<_>, _>>()?;
        resolve_call(name, bindings, scope.catalog())?;
        return Err(BindError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!(
                "OVER specified, but {name} is not a window function nor an aggregate function"
            ),
        ));
    };
    let ret = match &kind {
        WindowKind::Builtin { func, .. } => func.return_type(),
        WindowKind::Aggregate(agg) => agg.ret,
    };
    Ok(Binding::Typed(BoundExpr::WindowFunc {
        kind,
        spec: Box::new(spec),
        ret,
    }))
}

/// Whether a call site supplies exactly the arguments a dedicated window
/// function takes. All three take none, so this is an empty `()`.
///
/// The name alone does not make a call a window call: PG resolves by name *and*
/// argument types, so a user-defined `rank(int)` is an ordinary function and
/// only the zero-argument `rank()` is the builtin.
fn builtin_window_args_match(args: &ast::FunctionArguments) -> bool {
    match args {
        ast::FunctionArguments::List(list) => list.args.is_empty() && list.clauses.is_empty(),
        // `row_number` with no parentheses is a column reference, not a call.
        ast::FunctionArguments::None | ast::FunctionArguments::Subquery(_) => false,
    }
}

/// Resolve an `OVER` clause to the window specification it denotes.
///
/// `OVER w` *is* the named window, frame and all. `OVER (w …)` **copies** it,
/// and a copy is more restricted: it may add an `ORDER BY` only if the base has
/// none, may never add a `PARTITION BY`, and — because a copy takes the base's
/// rows but supplies its own frame — may not copy a base that has one. PG's hint
/// on that last error points at the difference: dropping the parentheses turns
/// the copy back into a reference, which is always allowed.
fn resolve_over_clause(
    over: &ast::WindowType,
    scope: &Scope,
) -> Result<ast::WindowSpec, BindError> {
    match over {
        // Every stored definition is already expanded, so a reference needs no
        // further resolution — it *is* the window, frame and all.
        ast::WindowType::NamedWindow(ident) => lookup_named_window(ident, scope).cloned(),
        ast::WindowType::WindowSpec(spec) => expand_window_base(
            spec.clone(),
            |name| scope.named_window(name),
            WindowCopyOrigin::Over,
        ),
    }
}

/// Where a window copy was written, which decides only whether PG's hint applies.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowCopyOrigin {
    /// `OVER (base …)` — the hint is "omit the parentheses", which turns the
    /// copy back into a reference.
    Over,
    /// `WINDOW w AS (base …)`, where there are no parentheses to omit.
    Definition,
}

/// Merge a window copy onto the base it names, or return it unchanged when it
/// names none.
///
/// The copy restrictions are PG's, and are identical in both positions: a copy
/// may add an `ORDER BY` only if the base has none, may never add a
/// `PARTITION BY`, and — because a copy takes the base's rows but supplies its
/// own frame — may not copy a base that has one.
pub(crate) fn expand_window_base<'a>(
    spec: ast::WindowSpec,
    lookup: impl Fn(&str) -> Option<&'a ast::WindowSpec>,
    origin: WindowCopyOrigin,
) -> Result<ast::WindowSpec, BindError> {
    let Some(base_name) = &spec.window_name else {
        return Ok(spec);
    };
    let key = crate::expr::normalize_ident(base_name);
    let Some(base) = lookup(&key) else {
        return Err(BindError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("window \"{key}\" does not exist"),
        ));
    };
    if !spec.partition_by.is_empty() {
        return Err(BindError::new(
            sqlstate::WINDOWING_ERROR,
            format!("cannot override PARTITION BY clause of window \"{key}\""),
        ));
    }
    if !spec.order_by.is_empty() && !base.order_by.is_empty() {
        return Err(BindError::new(
            sqlstate::WINDOWING_ERROR,
            format!("cannot override ORDER BY clause of window \"{key}\""),
        ));
    }
    if base.window_frame.is_some() {
        // The hint only makes sense for a copy that adds nothing — that is the
        // one that could have been written `OVER base` instead.
        let bare = origin == WindowCopyOrigin::Over
            && spec.order_by.is_empty()
            && spec.window_frame.is_none();
        return Err(BindError::new(
            sqlstate::WINDOWING_ERROR,
            format!("cannot copy window \"{key}\" because it has a frame clause"),
        )
        .with_hint(bare.then(|| "Omit the parentheses in this OVER clause.".to_string())));
    }
    Ok(ast::WindowSpec {
        window_name: None,
        partition_by: base.partition_by.clone(),
        order_by: if spec.order_by.is_empty() {
            base.order_by.clone()
        } else {
            spec.order_by
        },
        window_frame: spec.window_frame,
    })
}

/// The `WINDOW` definition `name` refers to, or PG's "does not exist".
fn lookup_named_window<'a>(
    name: &ast::Ident,
    scope: &'a Scope,
) -> Result<&'a ast::WindowSpec, BindError> {
    let key = crate::expr::normalize_ident(name);
    scope.named_window(&key).ok_or_else(|| {
        BindError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("window \"{key}\" does not exist"),
        )
    })
}

/// Bind a resolved `OVER (…)` specification against the pre-window row.
pub(crate) fn bind_window_spec(
    spec: &ast::WindowSpec,
    scope: &Scope,
) -> Result<BoundWindowSpec, BindError> {
    if let Some(frame) = &spec.window_frame
        && !is_default_frame(frame)
    {
        return Err(BindError::feature_not_supported(
            "explicit window frames are not supported yet",
        ));
    }
    let partition_by = spec
        .partition_by
        .iter()
        .map(|expr| bind_window_key(expr, "PARTITION BY", scope))
        .collect::<Result<Vec<_>, _>>()?;
    let order_by = spec
        .order_by
        .iter()
        .map(|item| {
            let expr = bind_window_key(&item.expr, "ORDER BY", scope)?;
            Ok(ExprSortKey::new(expr, &item.options))
        })
        .collect::<Result<Vec<_>, BindError>>()?;
    Ok(BoundWindowSpec {
        partition_by,
        order_by,
    })
}

/// Whether an explicitly written frame is exactly the one a spec gets by
/// default, `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`. Writing the
/// default out longhand is common, and accepting it costs one comparison.
fn is_default_frame(frame: &ast::WindowFrame) -> bool {
    matches!(frame.units, ast::WindowFrameUnits::Range)
        && matches!(frame.start_bound, ast::WindowFrameBound::Preceding(None))
        && matches!(
            frame.end_bound,
            None | Some(ast::WindowFrameBound::CurrentRow)
        )
        && matches!(
            frame.exclude,
            None | Some(ast::WindowFrameExclusion::NoOthers)
        )
}

/// Bind one `PARTITION BY` / `ORDER BY` expression of a window spec. Both are
/// compared by the executor — for partition boundaries and for peer groups —
/// so a type that cannot be ordered is refused here rather than reaching
/// `compare_values` and panicking, exactly as `bind_order_by` does.
fn bind_window_key(expr: &ast::Expr, clause: &str, scope: &Scope) -> Result<BoundExpr, BindError> {
    let bound = crate::expr::bind_scalar(expr, scope)?;
    // Aggregates are legal here (`WINDOW w AS (ORDER BY count(*))`), so this is
    // the window-only guard. The clause name is fixed rather than `PARTITION BY`
    // / `ORDER BY`, matching PG.
    crate::expr::reject_window(&bound, "window definitions")?;
    if !crate::expr::is_orderable(bound.ty(), scope.catalog().as_ref()) {
        return Err(BindError::feature_not_supported(format!(
            "window {clause} on type {} is not supported yet",
            crate::expr::type_label(bound.ty(), scope.catalog().as_ref())
        )));
    }
    Ok(bound)
}

/// Bind an aggregate call (`count(*)`, `min(x)`, `sum(a + b)`, …) to a transient
/// [`BoundExpr::Aggregate`] marker. The binder's extraction pass later moves it
/// into a [`crate::LogicalPlan::Aggregate`] node and replaces the marker with a
/// `ColumnRef`. `FILTER`/`OVER`/`WITHIN GROUP` were already rejected by the
/// caller; the per-aggregate `ORDER BY` is bound here, into the marker's
/// `order_by`.
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
    // The only argument-list clause an aggregate call may carry is its own
    // `ORDER BY`; anything else (a `LIMIT`, a `SEPARATOR`) is a dialect the
    // parser accepts and PG's grammar does not.
    let order_by = match list.clauses.as_slice() {
        [] => Vec::new(),
        [ast::FunctionArgumentClause::OrderBy(exprs)] => bind_aggregate_order_by(exprs, scope)?,
        _ => {
            return Err(BindError::feature_not_supported(
                "this function argument form is not supported yet",
            ));
        }
    };

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
            agg_args: Vec::new(),
            order_by,
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

    // Bind each argument so a wrong-arity error can name the actual argument
    // types, as PG does. The `Binding` is kept unsettled for one step because
    // `array_agg` is declared over `anyarray` *and* `anynonarray` in PG: an
    // `unknown` fits neither better, so it never resolves, and both
    // `array_agg(NULL)` and `array_agg($1)` are `42725` rather than the text
    // array the default would reach.
    let arg_exprs = positional_arg_exprs(&list.args)?;
    let bindings = arg_exprs
        .iter()
        .map(|e| crate::expr::bind_expr(e, scope))
        // An aggregate over a domain is the aggregate over its base, as for
        // any other call: `sum(posint_column)` is `sum(integer)`.
        .map(|b| b.map(|b| undomain_binding(b, scope.catalog().as_ref())))
        .collect::<Result<Vec<_>, _>>()?;
    if agg == AggFn::ArrayAgg
        && bindings
            .iter()
            .any(|b| matches!(b, Binding::Unknown { .. }))
    {
        return Err(ambiguous_function(name, &bindings));
    }
    let mut bound = bindings
        .into_iter()
        .map(crate::expr::scalar_from_binding)
        .collect::<Result<Vec<_>, _>>()?;
    // DISTINCT eliminates duplicates by sorting on the *arguments*, so PG only
    // accepts the spellings where that sort and this one coincide. Checked
    // against the arguments as bound, before the coercions below rewrite them.
    if distinct && order_by.iter().any(|key| !bound.contains(&key.expr)) {
        return Err(BindError::new(
            sqlstate::INVALID_COLUMN_REFERENCE,
            "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list",
        ));
    }
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
            agg_args: vec![value, delim],
            order_by,
            input_ty: PgType::Text,
            ret: agg_return_type(agg, PgType::Text, scope)?,
        }));
    }

    // Every other supported aggregate is unary.
    if bound.len() != 1 {
        return Err(undefined_arity());
    }
    let mut arg = bound.pop().expect("exactly one argument");
    let mut input_ty = arg.ty();
    // PG has no min/max aggregate for `"char"` (oid 18 appears in no `pg_proc`
    // min/max signature), so the argument resolves through the implicit
    // `"char" -> text` cast and both the ordering and the result type are
    // text's. Left as a byte, `max()` would order unsigned and return a
    // *different row's* value than PG — `'\377'` rather than `'Z'`.
    if matches!(agg, AggFn::Min | AggFn::Max) && input_ty == PgType::Char {
        arg = crate::expr::coerce_expr(arg, PgType::Text)?;
        input_ty = PgType::Text;
    }
    // PostgreSQL eliminates a DISTINCT aggregate's duplicates by *sorting*, so
    // an ordering is required and not only an equality — for every aggregate,
    // not just the one whose output makes it visible: `count(DISTINCT xid_col)`
    // is this same error. The two checks are in PG's order, which is what
    // decides the message for a type that has neither (`point`, `json`) versus
    // `xid`, the one type in the gap (see `has_equality`).
    if distinct {
        let catalog = scope.catalog();
        if !crate::expr::has_equality(input_ty, catalog.as_ref()) {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "could not identify an equality operator for type {}",
                    crate::expr::type_label(input_ty, catalog.as_ref())
                ),
            ));
        }
        if !crate::expr::is_orderable(input_ty, catalog.as_ref()) {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "could not identify an ordering operator for type {}",
                    crate::expr::type_label(input_ty, catalog.as_ref())
                ),
            )
            .with_detail(Some(
                "Aggregates with DISTINCT must be able to sort their inputs.".to_string(),
            )));
        }
    }
    let ret = agg_return_type(agg, input_ty, scope)?;
    Ok(Binding::Typed(BoundExpr::Aggregate {
        func: agg,
        distinct,
        agg_args: vec![arg],
        order_by,
        input_ty,
        ret,
    }))
}

/// Bind an aggregate's own `ORDER BY` (`array_agg(x ORDER BY y DESC)`).
///
/// The keys are evaluated against the same source row as the arguments, so they
/// bind exactly as a window spec's do (see [`bind_window_spec`]) — including the
/// orderable-type check, without which a key would reach `compare_values` and
/// panic. A nested aggregate or window call in a key is caught later, by the
/// extraction pass that already asks the same of the arguments.
fn bind_aggregate_order_by(
    exprs: &[ast::OrderByExpr],
    scope: &Scope,
) -> Result<Vec<ExprSortKey>, BindError> {
    exprs
        .iter()
        .map(|item| {
            let expr = crate::expr::bind_scalar(&item.expr, scope)?;
            if !crate::expr::is_orderable(expr.ty(), scope.catalog().as_ref()) {
                return Err(BindError::new(
                    sqlstate::UNDEFINED_FUNCTION,
                    format!(
                        "could not identify an ordering operator for type {}",
                        crate::expr::type_label(expr.ty(), scope.catalog().as_ref())
                    ),
                )
                .with_detail(Some(
                    "Use an explicit ordering operator or modify the query.".to_string(),
                )));
            }
            Ok(ExprSortKey::new(expr, &item.options))
        })
        .collect()
}

/// Bind `CURRENT_DATE`, `CURRENT_TIME(p)`, `CURRENT_TIMESTAMP(p)`,
/// `LOCALTIME(p)` and `LOCALTIMESTAMP(p)`. `None` for any other name.
///
/// Every one of them is exactly a cast of `now()` — verified against
/// PostgreSQL 18.4 in several session zones: `current_date = now()::date`,
/// `localtimestamp = now()::timestamp`, `current_time = now()::timetz`,
/// `localtime = now()::time`, `current_time(2) = now()::timetz(2)`. Building
/// them that way rather than as five more `ScalarFn`s means the zone rules and
/// the rounding rule live in one place each, and cannot drift apart.
///
/// These are grammar productions, not `pg_proc` entries, so only the bare
/// unquoted spelling is one — see the guard on `object_name` below.
fn bind_current_datetime(
    name: &str,
    object_name: &ast::ObjectName,
    args: &ast::FunctionArguments,
) -> Result<Option<Binding>, BindError> {
    // Only the *keyword* spelling is a keyword. `name` has already been
    // lowercased and stripped of quoting, so it cannot tell `current_date` from
    // `"current_date"`; the unnormalized name can. A quoted word carries a
    // `quote_style` and is never a keyword to the tokenizer, and a qualified
    // name has more than one part — neither is the grammar production, so both
    // must fall through to ordinary function resolution.
    //
    // Without this, any function whose bare name matched was intercepted:
    // `CREATE FUNCTION "localtime"(int)` became unreachable, and its argument
    // was reinterpreted as a fractional-second precision.
    let [part] = object_name.0.as_slice() else {
        return Ok(None);
    };
    if !part.as_ident().is_some_and(|i| i.quote_style.is_none()) {
        return Ok(None);
    }
    let target = match name {
        "current_timestamp" => None,
        "localtimestamp" => Some(PgType::Timestamp),
        "current_date" => Some(PgType::Date),
        "current_time" => Some(PgType::TimeTz),
        "localtime" => Some(PgType::Time),
        _ => return Ok(None),
    };
    let now = BoundExpr::FuncCall {
        func: ScalarFn::TransactionTimestamp,
        ret: PgType::TimestampTz,
        args: Vec::new(),
    };
    let expr = match target {
        None => now,
        Some(ty) => crate::expr::coerce_expr(now, ty)?,
    };
    // The grammar has already rejected everything but a bare integer literal
    // here, so a modifier is either absent or well-formed. `CURRENT_DATE` is
    // rejected there too — repeated because a `date` has no fractional seconds
    // to round, and `apply_datetime_precision` would hand the executor a
    // `TimeApplyTypmod` over a `date` that it can only panic on.
    let expr = match keyword_precision(args)? {
        None => expr,
        Some(_) if target == Some(PgType::Date) => {
            return Err(BindError::syntax("syntax error at or near \"(\""));
        }
        Some(p) => crate::expr::apply_datetime_precision(expr, p)?,
    };
    Ok(Some(Binding::Typed(expr)))
}

/// The `(p)` of a keyword datetime form, clamped like a written `timestamp(p)`
/// type modifier — see [`crate::expr::datetime_precision`] for the missing
/// `WARNING` that clamping inherits.
fn keyword_precision(args: &ast::FunctionArguments) -> Result<Option<i32>, BindError> {
    let ast::FunctionArguments::List(list) = args else {
        return Ok(None);
    };
    let [ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(expr))] = list.args.as_slice() else {
        return Err(BindError::syntax("syntax error at or near \"(\""));
    };
    let ast::Expr::Value(v) = expr else {
        return Err(BindError::syntax("syntax error at or near \"(\""));
    };
    let ast::Value::Number(digits, _) = &v.value else {
        return Err(BindError::syntax("syntax error at or near \"(\""));
    };
    let p: i64 = crate::expr::literal_int(digits)
        .and_then(|v| i64::try_from(v).ok())
        .ok_or_else(|| BindError::syntax("syntax error at or near \"(\""))?;
    Ok(Some(
        (p.min(i32::MAX as i64) as i32).min(crabgresql_types::timestamp::MAX_PRECISION),
    ))
}

/// Build a `FuncCall` binding, rejecting conflicting explicit `COLLATE`
/// clauses among `args` first (`concat('a' COLLATE x, 'b' COLLATE y)` is
/// `42P22` the same way `a COLLATE x = b COLLATE y` is). Shared by every
/// `FuncCall` construction site so the check isn't duplicated at each one.
fn finish_func_call(
    func: ScalarFn,
    ret: PgType,
    args: Vec<BoundExpr>,
) -> Result<Binding, BindError> {
    if ret.is_collatable() || args.iter().any(|a| a.ty().is_collatable()) {
        crate::collation::check_explicit_conflict(
            args.iter().map(crate::collation::expr_collation),
        )?;
    }
    Ok(Binding::Typed(BoundExpr::FuncCall { func, ret, args }))
}

/// Resolve `pg_index_has_property(index, prop)` and
/// `pg_index_column_has_property(index, column, prop)`, or `Ok(None)` for any
/// other name so the caller falls through to ordinary resolution.
///
/// PostgreSQL declares the index parameter `regclass` and accepts an `oid`
/// argument anyway, the two types being binary-coercible there; here they are not
/// (see [`resolve_partition_ancestors`] for why). Both spellings have to work —
/// a client writes `'i'::regclass` in a hand-typed query and
/// `pg_index.indexrelid` in a generated one — and two entries in the signature
/// table would not do it: an unadorned `pg_index_has_property('i', 'index_scan')`
/// would then be `42725 is not unique`, since an unknown literal fits both
/// candidates equally well. So the two parameter types are *tried in order*,
/// which also keeps a bare literal resolving **by name** rather than through
/// `text_to_oid`.
fn resolve_index_property(name: &str, bindings: &[Binding]) -> Result<Option<Binding>, BindError> {
    let tail: &[PgType] = match (name, bindings.len()) {
        ("pg_index_has_property", 2) => &[TEXT],
        ("pg_index_column_has_property", 3) => &[I4, TEXT],
        _ => return Ok(None),
    };
    let func = match name {
        "pg_index_has_property" => ScalarFn::PgIndexHasProperty,
        _ => ScalarFn::PgIndexColumnHasProperty,
    };
    for first in [REGCLASS, OID] {
        let params: Vec<PgType> = std::iter::once(first).chain(tail.iter().copied()).collect();
        if let Ok(args) = single_candidate_args(bindings, &params) {
            return finish_func_call(func, BOOL, args).map(Some);
        }
    }
    Err(undefined_function(name, bindings))
}

/// Resolve the four relation-size functions, or `Ok(None)` for any other name.
///
/// Their relation parameter is spelled `regclass` or `oid` for exactly the
/// reason [`resolve_index_property`] documents — a client writes
/// `pg_relation_size('t')` by hand and `pg_relation_size(c.oid)` in a query over
/// `pg_class`, and two signature-table entries would make the first `42725`.
///
/// `pg_size_pretty` is deliberately *not* here: its two overloads take a number
/// rather than a relation, and PostgreSQL really does raise `42725` for
/// `pg_size_pretty(1024)`, which the ordinary table gives for free.
fn resolve_relation_size(name: &str, bindings: &[Binding]) -> Result<Option<Binding>, BindError> {
    let tail: &[PgType] = match (name, bindings.len()) {
        ("pg_relation_size", 1) => &[],
        // The fork name: `main`, `fsm`, `vm` or `init`.
        ("pg_relation_size", 2) => &[TEXT],
        ("pg_table_size" | "pg_indexes_size" | "pg_total_relation_size", 1) => &[],
        _ => return Ok(None),
    };
    let func = match name {
        "pg_relation_size" => ScalarFn::PgRelationSize,
        "pg_table_size" => ScalarFn::PgTableSize,
        "pg_indexes_size" => ScalarFn::PgIndexesSize,
        _ => ScalarFn::PgTotalRelationSize,
    };
    for first in [REGCLASS, OID] {
        let params: Vec<PgType> = std::iter::once(first).chain(tail.iter().copied()).collect();
        if let Ok(args) = single_candidate_args(bindings, &params) {
            return finish_func_call(func, I8, args).map(Some);
        }
    }
    Err(undefined_function(name, bindings))
}

pub(crate) fn resolve_call(
    name: &str,
    bindings: Vec<Binding>,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<Binding, BindError> {
    resolve_call_variadic(name, bindings, catalog, false)
}

/// `variadic_call` is whether the call's last argument was written
/// `VARIADIC expr`. It changes only which shape a user routine's variadic
/// parameter presents (see [`routine_params`]); a built-in signature has no
/// variadic parameter to reshape, so the keyword just leaves the argument's own
/// array type to resolve against — which is exactly PostgreSQL's behavior for
/// `cardinality(VARIADIC a)` and `length(VARIADIC a)`'s 42883 alike.
pub(crate) fn resolve_call_variadic(
    name: &str,
    bindings: Vec<Binding>,
    catalog: &Arc<dyn TypeCatalog>,
    variadic_call: bool,
) -> Result<Binding, BindError> {
    // A function call over a domain resolves on the base type, as an operator
    // does — `length(v3col)` is `length(varchar)`. Both the built-in overload
    // table below and the user-routine fallback see the stripped arguments.
    let bindings: Vec<Binding> = bindings
        .into_iter()
        .map(|b| undomain_binding(b, catalog.as_ref()))
        .collect();
    let sigs = lookup(name);
    if sigs.is_empty() {
        // No built-in of this name: a user-defined `LANGUAGE SQL` function may
        // still match. Only when that also fails is the call undefined.
        return resolve_user_routine_call(
            name,
            bindings,
            catalog.routines(name),
            catalog,
            variadic_call,
        );
    }
    // First try an all-exact-type match. Then, among the signatures whose args
    // all coerce, pick the one keeping the most arguments at their exact type —
    // so `power(numeric, int)` prefers `power(numeric, numeric)` over the float8
    // overload — and then the ones converting to their category's preferred
    // type, which is how `abs(int4)` reaches the exact int4 overload rather than
    // the float8 one.
    // PG chooses an overload from the argument *types*, never from an untyped
    // literal's contents, so the unknown-argument rule below considers every
    // same-arity signature — not just the ones whose literal happens to parse.
    let arity: Vec<&Signature> = sigs
        .iter()
        .filter(|sig| sig.args.len() == bindings.len())
        .collect();
    let candidates = if arity.len() > 1 {
        // The typed arguments get the first and last word: PG discards the
        // candidates they cannot reach and keeps the most exact matches among
        // them (rules 4.a/4.b) *before* consulting the unknown positions (4.e).
        // Running the unknown rule first would let the string-category
        // preference throw away a signature the typed arguments had already
        // singled out — `overlay(bit, unknown, int4)` would lose its `bit`
        // overload to the `text` one.
        let narrowed = narrow_by_typed_args(&bindings, arity);
        let has_unknown = bindings
            .iter()
            .any(|b| matches!(b, Binding::Unknown { .. }));
        if narrowed.len() > 1 && has_unknown {
            narrow_by_unknown_category(name, &bindings, narrowed)?
        } else {
            // With nothing left to separate typed candidates PG gives up
            // rather than picking one: `gcd(int2, int2)` is `42725` because
            // smallint reaches the int4, int8 and numeric overloads alike and
            // none of them is the numeric category's preferred type.
            //
            // `exact_only` is false throughout — it is the pass that admits an
            // implicit cast, and with it true a widening argument reaches
            // nothing and every such call would look unambiguous.
            let reachable = |args: &[PgType]| !typed_mismatch(&bindings, args, false);
            // Recounted rather than taken from `narrowed`, whose reachability
            // is `implicit_castable` where resolution below is
            // `coerce_for_arg`: if the two disagree there is only one candidate
            // and no ambiguity.
            let survivors = narrowed.iter().filter(|sig| reachable(sig.args)).count();
            // A routine PG would have weighed suppresses the error.
            let user_candidate = catalog.routines(name).iter().any(|r| {
                routine_params(r, bindings.len(), variadic_call).is_some_and(|p| reachable(&p))
            });
            if survivors > 1 && !user_candidate {
                return Err(ambiguous_function(name, &bindings));
            }
            narrowed
        }
    } else {
        arity
    };
    for sig in &candidates {
        if let Ok(args) = try_coerce_args(&bindings, sig.args, true) {
            return finish_func_call(sig.func, sig.ret, args);
        }
    }
    let mut best: Option<(usize, &Signature, Vec<BoundExpr>)> = None;
    // The rejected literal of a lone candidate, kept from the pass that found
    // it so the reporting path below need not parse it a second time.
    let mut literal_fail: Option<BindError> = None;
    for sig in &candidates {
        match try_coerce_args(&bindings, sig.args, false) {
            Ok(args) => {
                let exact = bindings
                    .iter()
                    .zip(sig.args)
                    .filter(|(b, target)| matches!(b, Binding::Typed(e) if e.ty() == **target))
                    .count();
                if best.as_ref().is_none_or(|(b, _, _)| exact > *b) {
                    best = Some((exact, sig, args));
                }
            }
            Err(ArgFail::LiteralInput(e)) if candidates.len() == 1 => literal_fail = Some(e),
            Err(_) => {}
        }
    }
    match best {
        Some((_, sig, args)) => finish_func_call(sig.func, sig.ret, args),
        None => {
            // No built-in overload fit the argument types; a user `LANGUAGE SQL`
            // function of the same name may still match.
            let routines = catalog.routines(name);
            // PG picks the overload from the argument types alone and runs the
            // input functions only afterwards, so once one candidate is the
            // only one left, its literal's failure is the answer: `crc32('\x4')`
            // is the odd-digit error, not "function crc32(unknown) does not
            // exist". Same-arity user routines are still candidates by type, so
            // they have to be ruled out first; other arities never were.
            if let Some(e) = literal_fail
                && !routines
                    .iter()
                    .any(|r| routine_params(r, bindings.len(), variadic_call).is_some())
            {
                return Err(e);
            }
            resolve_user_routine_call(name, bindings, routines, catalog, variadic_call)
        }
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
/// `sigs` is passed in rather than fetched: [`TypeCatalog::routines`] takes the
/// catalog lock and deep-clones every overload, body text included, so the one
/// caller that already needed the list must not make this fetch it again.
fn resolve_user_routine_call(
    name: &str,
    bindings: Vec<Binding>,
    sigs: Vec<RoutineSig>,
    catalog: &Arc<dyn TypeCatalog>,
    variadic_call: bool,
) -> Result<Binding, BindError> {
    let Some((sig, args)) =
        choose_routine_overload(name, &bindings, &sigs, variadic_call, catalog)?
    else {
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

    // Report the *chosen overload* to anyone tracking dependencies, but only at
    // the top level: a routine reached from inside an inlined SQL body is not a
    // dependency in PostgreSQL, and recording it would over-block `DROP
    // FUNCTION`. Placed before the inlining below so it runs for both routine
    // kinds and cannot be skipped by an early return further down.
    if INLINE_DEPTH.with(|d| d.get()) == 0 {
        catalog.note_routine_use(sig.oid);
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

/// The parameter list a routine presents to a call of `nargs` arguments, or
/// `None` when it cannot take that many.
///
/// A `VARIADIC` routine has *two* shapes, and the call picks which one applies.
/// Written `f(1, 2, 3)`, the trailing parameter stands for however many
/// arguments are left, so `f(a int, VARIADIC b int[])` presents `int, int, int`.
/// Written `f(1, VARIADIC arr)`, the array *is* that parameter and the declared
/// list is used as it stands. PostgreSQL admits only the shape the call chose:
/// `f(ARRAY[1,2])` against `f(VARIADIC int[])` is `42883`, verified on 18.4.
///
/// The spread shape needs at least one argument for the variadic parameter —
/// `f()` against `f(VARIADIC int[])` is `42883` too, not an empty array.
pub fn routine_params(sig: &RoutineSig, nargs: usize, variadic_call: bool) -> Option<Vec<PgType>> {
    let declared = &sig.arg_types;
    match sig.variadic_elem {
        Some(elem) if !variadic_call => {
            if nargs < declared.len() {
                return None;
            }
            let mut params = declared[..declared.len() - 1].to_vec();
            params.resize(nargs, elem);
            Some(params)
        }
        _ => (declared.len() == nargs).then(|| declared.clone()),
    }
}

/// A spread call's trailing arguments folded back into the array its variadic
/// parameter actually receives; a no-op for every other call shape.
fn pack_variadic_tail(
    sig: &RoutineSig,
    variadic_call: bool,
    mut args: Vec<BoundExpr>,
) -> Vec<BoundExpr> {
    let Some(elem) = sig.variadic_elem.filter(|_| !variadic_call) else {
        return args;
    };
    let tail = args.split_off(sig.arg_types.len() - 1);
    args.push(BoundExpr::ArrayCtor {
        elem,
        ty: PgType::Array(elem.oid()),
        elems: tail,
    });
    args
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
    variadic_call: bool,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<Option<(&'a RoutineSig, Vec<BoundExpr>)>, BindError> {
    let spread = |sig: &RoutineSig| sig.variadic_elem.is_some() && !variadic_call;
    // Each candidate carries its parameter list twice: as declared, which is
    // what an argument finally enters, and resolved on the base, which is what
    // it is matched and scored against. A parameter declared on a domain
    // accepts whatever its base accepts, and the arguments arrived
    // base-stripped (see `resolve_call_variadic`), so matching them against a
    // declared `PgType::User(oid)` would find no overload at all.
    let mut candidates: Vec<(&RoutineSig, Vec<PgType>, Vec<PgType>)> = sigs
        .iter()
        .filter_map(|sig| {
            let declared = routine_params(sig, bindings.len(), variadic_call)?;
            let base = declared.iter().map(|t| catalog.base_type(*t)).collect();
            Some((sig, declared, base))
        })
        .collect();
    // `m(VARIADIC int[])` and `m(int)` both present `int` to `m(1)`, and
    // PostgreSQL calls the second whichever order they were created in. The
    // test is on the *lists*, not on variadic-ness: where they differ both stay
    // and the ordinary rules decide, which is how `n(VARIADIC int[])` beats
    // `n(numeric)` for `n(1)`. Two spread candidates collapsing onto each other
    // keep competing, and so are `42725`. All four verified on 18.4.
    let fixed: Vec<Vec<PgType>> = candidates
        .iter()
        .filter(|(sig, _, _)| !spread(sig))
        .map(|(_, _, base)| base.clone())
        .collect();
    candidates.retain(|(sig, _, base)| !spread(sig) || !fixed.contains(base));

    // Two overloads can never share the same argument types (`create_function`
    // rejects that), and the dedup above closed the one other way two
    // candidates could present the same list, so at most one all-exact match
    // exists.
    for (sig, declared, base) in &candidates {
        if let Ok(args) = coerce_routine_args(bindings, declared, base, true, catalog) {
            return Ok(Some((sig, pack_variadic_tail(sig, variadic_call, args))));
        }
    }
    // No exact match: rank the coercible candidates by how many arguments are
    // already at their exact type, as the built-in resolver does.
    let mut best: Option<(usize, &RoutineSig, Vec<BoundExpr>)> = None;
    let mut tied = false;
    let mut literal_fail: Option<BindError> = None;
    for (sig, declared, base) in &candidates {
        let args = match coerce_routine_args(bindings, declared, base, false, catalog) {
            Ok(args) => args,
            // As for built-ins: the rejected literal of a lone candidate is the
            // error PG reports, since the overload was already chosen by then.
            Err(ArgFail::LiteralInput(e)) if candidates.len() == 1 => {
                literal_fail = Some(e);
                continue;
            }
            Err(_) => continue,
        };
        let score = bindings
            .iter()
            .zip(base)
            .filter(|(b, target)| matches!(b, Binding::Typed(e) if e.ty() == **target))
            .count();
        let args = pack_variadic_tail(sig, variadic_call, args);
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
    if best.is_none()
        && let Some(e) = literal_fail
    {
        return Err(e);
    }
    Ok(best.map(|(_, sig, args)| (sig, args)))
}

/// Coerce a call's arguments to one candidate's parameter list.
///
/// `base` is what each argument is matched and coerced against — a domain
/// parameter accepts whatever its base does — and `declared` is what the
/// coerced argument then enters, so a domain parameter's constraints run on the
/// call. PostgreSQL enforces them there: `f(-1)` on `f(p posint)` raises 23514
/// (probed on 18.4).
///
/// A domain violation folded at bind time reports as [`ArgFail::LiteralInput`],
/// the way a rejected literal already travels: it surfaces only when the
/// candidate was the sole one, and is otherwise just a candidate that did not
/// fit.
fn coerce_routine_args(
    bindings: &[Binding],
    declared: &[PgType],
    base: &[PgType],
    exact_only: bool,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<Vec<BoundExpr>, ArgFail> {
    let args = try_coerce_args(bindings, base, exact_only)?;
    args.into_iter()
        .zip(declared)
        .map(
            |(arg, declared)| match domain_of(*declared, catalog.as_ref()) {
                Some(info) => {
                    wrap_domain(arg, &info, catalog, false).map_err(ArgFail::LiteralInput)
                }
                None => Ok(arg),
            },
        )
        .collect()
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
/// SELECT target list (`generate_series`, `jsonb_path_query`, `unnest`), bind it
/// to a [`BoundExpr::Srf`] marker. Returns `Ok(None)` when it is not such a
/// call, so the caller can bind it as an ordinary scalar instead.
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
    if name != "generate_series"
        && name != "jsonb_path_query"
        && name != "unnest"
        && name != "generate_subscripts"
        && name != "pg_partition_ancestors"
    {
        return Ok(None);
    }
    let arg_exprs = positional_args(&func.args)?;
    let bindings = arg_exprs
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    // The target-list spelling psql's `\d` uses: `SELECT
    // pg_partition_ancestors(oid) UNION ALL VALUES (oid)`.
    if name == "pg_partition_ancestors" {
        return Ok(Some(BoundExpr::Srf {
            func: TableFn::PgPartitionAncestors,
            ret: PgType::Reg(RegKind::Class),
            args: resolve_partition_ancestors(&bindings)?,
        }));
    }
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
    if name == "generate_subscripts" {
        return Ok(Some(BoundExpr::Srf {
            func: TableFn::GenerateSubscripts,
            ret: PgType::Int4,
            args: resolve_generate_subscripts(&bindings)?,
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
    match single_candidate_args(bindings, params) {
        Ok(args) => Ok(args),
        Err(None) => Err(undefined_function("jsonb_path_query", bindings)),
        Err(Some(e)) => Err(e),
    }
}

/// Coerce a call against the one signature that could possibly match, exact
/// pass first. `Err(Some(e))` is a literal its input function rejected, which
/// PG reports because the overload was never in doubt; `Err(None)` leaves the
/// caller to raise its own `42883`.
fn single_candidate_args(
    bindings: &[Binding],
    params: &[PgType],
) -> Result<Vec<BoundExpr>, Option<BindError>> {
    let mut literal_fail = None;
    for exact_only in [true, false] {
        match try_coerce_args(bindings, params, exact_only) {
            Ok(args) => return Ok(args),
            Err(ArgFail::LiteralInput(e)) => literal_fail = Some(e),
            Err(_) => {}
        }
    }
    Err(literal_fail)
}

/// Try to coerce every binding to the signature's parameter types. When
/// `exact_only`, reject anything that would need a numeric promotion.
///
/// A rejected literal is only reported as such when no *typed* argument also
/// fails, since PG would then have dropped the candidate on types alone and
/// never reached the literal. Scanning that tail is deliberately limited to
/// typed arguments: continuing the normal loop past a failure would resolve
/// later `$n` placeholders and record type deductions for a signature that is
/// not going to win.
fn try_coerce_args(
    bindings: &[Binding],
    params: &[PgType],
    exact_only: bool,
) -> Result<Vec<BoundExpr>, ArgFail> {
    let mut out = Vec::with_capacity(params.len());
    for (i, (binding, &target)) in bindings.iter().zip(params).enumerate() {
        match coerce_for_arg(binding.clone(), target, exact_only) {
            Ok(arg) => out.push(arg),
            Err(ArgFail::LiteralInput(e)) => {
                let rest = i + 1;
                return Err(
                    if typed_mismatch(&bindings[rest..], &params[rest..], exact_only) {
                        ArgFail::Mismatch
                    } else {
                        ArgFail::LiteralInput(e)
                    },
                );
            }
            Err(other) => return Err(other),
        }
    }
    Ok(out)
}

/// Whether any *typed* argument fails to reach its parameter type. Unknown
/// bindings are skipped, so this is free of the `ParamCtx` side effect that
/// makes [`coerce_for_arg`] unsafe to call speculatively.
fn typed_mismatch(bindings: &[Binding], params: &[PgType], exact_only: bool) -> bool {
    bindings.iter().zip(params).any(|(binding, &target)| {
        matches!(binding, Binding::Typed(_))
            && coerce_for_arg(binding.clone(), target, exact_only).is_err()
    })
}

/// PG's `42804` for `concat(VARIADIC 10)`: a `VARIADIC "any"` parameter spreads
/// an array's elements, so the argument has to *be* an array. The caret sits on
/// the argument, not on the `VARIADIC` keyword.
fn variadic_not_array(span: crabgresql_parser::Span) -> BindError {
    BindError::new(
        sqlstate::DATATYPE_MISMATCH,
        "VARIADIC argument must be an array",
    )
    .at_if_unset(span)
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

/// PG's `pg_type.typcategory`, transcribed from `vendor/postgres/catalog/
/// pg_type.dat`. Types sharing a category are interchangeable enough that an
/// untyped literal can be steered between them; types in different categories
/// are not, and an untyped argument spanning two of them is ambiguous.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    /// `B`.
    Boolean,
    /// `S`.
    String,
    /// `N`.
    Numeric,
    /// `D`.
    DateTime,
    /// `T`.
    Timespan,
    /// `I`.
    Network,
    /// `V`.
    BitString,
    /// `G`.
    Geometric,
    /// `U`, PG's catch-all for types with no family of their own.
    UserDefined,
    /// A type not in the table above (currently the `reg*` family and arrays):
    /// its own category, keyed by OID so unrelated types never look alike.
    Other(u32),
}

fn category(ty: PgType) -> Category {
    match ty {
        PgType::Bool => Category::Boolean,
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => Category::String,
        PgType::Int2
        | PgType::Int4
        | PgType::Int8
        | PgType::Float4
        | PgType::Float8
        | PgType::Numeric
        | PgType::Money
        | PgType::Oid => Category::Numeric,
        PgType::Date | PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
            Category::DateTime
        }
        PgType::Interval => Category::Timespan,
        PgType::Inet | PgType::Cidr => Category::Network,
        PgType::Bit | PgType::Varbit => Category::BitString,
        PgType::Point
        | PgType::Lseg
        | PgType::Line
        | PgType::Path
        | PgType::Box
        | PgType::Polygon
        | PgType::Circle => Category::Geometric,
        PgType::Bytea
        | PgType::Uuid
        | PgType::Json
        | PgType::Jsonb
        | PgType::Jsonpath
        | PgType::Tsvector
        | PgType::Tsquery
        | PgType::Macaddr
        | PgType::Macaddr8
        | PgType::Tid
        | PgType::Xid
        | PgType::Xid8
        | PgType::Cid
        | PgType::PgLsn => Category::UserDefined,
        other => Category::Other(other.oid()),
    }
}

/// `pg_type.typispreferred`, from the same source. A category can have more
/// than one (`N` marks both `float8` and `oid`), and several have none.
fn is_preferred(ty: PgType) -> bool {
    matches!(
        ty,
        PgType::Bool
            | PgType::Text
            | PgType::Float8
            | PgType::Oid
            | PgType::TimestampTz
            | PgType::Interval
            | PgType::Inet
            | PgType::Varbit
    )
}

/// PG's rules 4.a and 4.b, restricted to the arguments whose type is already
/// known: drop the candidates no typed argument can reach, then keep those with
/// the most arguments already at their exact type. An empty result is left empty
/// so the caller reports `42883` rather than a misleading ambiguity.
fn narrow_by_typed_args<'a>(
    bindings: &[Binding],
    candidates: Vec<&'a Signature>,
) -> Vec<&'a Signature> {
    let reachable = |sig: &&Signature| {
        bindings
            .iter()
            .zip(sig.args)
            .all(|(binding, target)| match binding {
                // An untyped literal reaches anything; it is the unknown-argument
                // rule's job to choose between the candidates it leaves standing.
                Binding::Unknown { .. } => true,
                Binding::Typed(e) => {
                    e.ty() == *target || crate::expr::implicit_castable(e.ty(), *target)
                }
            })
    };
    let exact_matches = |sig: &&Signature| {
        bindings
            .iter()
            .zip(sig.args)
            .filter(|(binding, target)| matches!(binding, Binding::Typed(e) if e.ty() == **target))
            .count()
    };
    // PG's 4.d: among the arguments that still need converting, prefer the
    // candidates taking their category's preferred type. This is what makes
    // `to_char(date, unknown)` the timestamptz overload rather than a tie with
    // the timestamp one.
    let preferred_conversions = |sig: &&Signature| {
        bindings
            .iter()
            .zip(sig.args)
            .filter(|(binding, target)| match binding {
                Binding::Unknown { .. } => false,
                Binding::Typed(e) => {
                    e.ty() != **target
                        && is_preferred(**target)
                        && category(**target) == category(e.ty())
                }
            })
            .count()
    };
    let mut kept: Vec<&Signature> = candidates.into_iter().filter(reachable).collect();
    let best = kept.iter().map(exact_matches).max().unwrap_or(0);
    kept.retain(|sig| exact_matches(&sig) == best);
    let best = kept.iter().map(preferred_conversions).max().unwrap_or(0);
    kept.retain(|sig| preferred_conversions(&sig) == best);
    kept
}

/// PG's unknown-argument rule. When several overloads all match exactly, the
/// only thing separating them is what an untyped literal should become: at each
/// unknown position prefer the string category (an unknown literal *looks* like
/// a string), else require every candidate to agree on one category, else give
/// up with `42725`. Within the chosen category the preferred type wins.
///
/// This is what makes `substring('abcdef','2')` pick the regex form (a `text`
/// candidate exists at position 2) while `to_char('x','y')` is ambiguous (its
/// candidates span the datetime, timespan and numeric categories).
fn narrow_by_unknown_category<'a>(
    name: &str,
    bindings: &[Binding],
    mut candidates: Vec<&'a Signature>,
) -> Result<Vec<&'a Signature>, BindError> {
    for (i, binding) in bindings.iter().enumerate() {
        if candidates.len() < 2 {
            break;
        }
        if !matches!(binding, Binding::Unknown { .. }) {
            continue;
        }
        let mut cats = candidates.iter().map(|sig| category(sig.args[i]));
        let first = match cats.next() {
            Some(c) => c,
            None => break,
        };
        let chosen = if candidates
            .iter()
            .any(|sig| category(sig.args[i]) == Category::String)
        {
            Category::String
        } else if cats.all(|c| c == first) {
            first
        } else {
            return Err(ambiguous_function(name, bindings));
        };
        candidates.retain(|sig| category(sig.args[i]) == chosen);
        if candidates.iter().any(|sig| is_preferred(sig.args[i])) {
            candidates.retain(|sig| is_preferred(sig.args[i]));
        }
    }
    // Nothing is left to separate the survivors: the category agreed and no
    // preferred type broke the tie, so PG gives up rather than picking one.
    // Categories with no preferred type at all — `G`, `U` — always land here,
    // which is why `area('((0,0),(2,2))')` is ambiguous in PG.
    if candidates.len() > 1 {
        return Err(ambiguous_function(name, bindings));
    }
    Ok(candidates)
}

/// PG's `42725` for a call that matches two overloads equally well. Unlike the
/// operator-ambiguity error in [`crate::expr`], which splits its advice across
/// DETAIL and HINT, PG puts the whole sentence in the function form's HINT.
fn ambiguous_function(name: &str, bindings: &[Binding]) -> BindError {
    BindError::new(
        sqlstate::AMBIGUOUS_FUNCTION,
        format!(
            "function {name}({}) is not unique",
            call_type_list(bindings)
        ),
    )
    .with_hint(Some(
        "Could not choose a best candidate function. You might need to add explicit type casts."
            .to_string(),
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
///
/// A `VARIADIC expr` argument yields `expr` with no trace left behind, which is
/// all most callees need (see [`resolve_call_variadic`]). The two that need
/// more — a user routine's variadic parameter and a `VARIADIC "any"` built-in —
/// ask separately, via [`variadic_arg_index`].
pub(crate) fn positional_arg_exprs(args: &[ast::FunctionArg]) -> Result<Vec<ast::Expr>, BindError> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e))
            | ast::FunctionArg::Variadic(ast::FunctionArgExpr::Expr(e)) => out.push(e.clone()),
            _ => {
                return Err(BindError::feature_not_supported(
                    "named or wildcard function arguments are not supported yet",
                ));
            }
        }
    }
    Ok(out)
}

/// Where the call wrote `VARIADIC`, if it did. The parser has already enforced
/// that it is the last argument.
fn variadic_arg_index(args: &ast::FunctionArguments) -> Option<usize> {
    let ast::FunctionArguments::List(list) = args else {
        return None;
    };
    list.args
        .iter()
        .position(|a| matches!(a, ast::FunctionArg::Variadic(_)))
}
