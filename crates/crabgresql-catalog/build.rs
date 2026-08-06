//! Build-time codegen for `pg_catalog`'s built-in rows.
//!
//! Reads PostgreSQL's vendored catalog *data* files
//! (`vendor/postgres/catalog/*.dat`) and emits typed row arrays the crate
//! includes at compile time. The `.dat` format is a Perl array-of-hashes; the
//! parser here reads that DATA only — it is an original scanner, NOT a port of
//! PostgreSQL's `Catalog.pm`. Fields a `.dat` entry omits are filled from the
//! per-catalog default tables below (authored from the public catalog docs),
//! so codegen never reads PostgreSQL's C headers.
//!
//! See `docs/ARCHITECTURE.md` §7 and `AGENTS.md`: vendoring catalog `.dat` data
//! and generating from it is the sanctioned path; attribution is in `NOTICE`.

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let catalog_dir = manifest.join("../../vendor/postgres/catalog");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    let type_entries = read_dat(&catalog_dir, "pg_type.dat")?;
    let cast_entries = read_dat(&catalog_dir, "pg_cast.dat")?;
    let proc_entries = read_dat(&catalog_dir, "pg_proc.dat")?;

    // Base type name -> OID, used to resolve name references in pg_type
    // (`typelem`) and pg_cast (`castsource`/`casttarget`).
    let mut name_to_oid: HashMap<String, u32> = type_entries
        .iter()
        .filter_map(|e| Some((get(e, "typname")?.to_string(), oid_field(e, "oid"))))
        .collect();
    // Array types have no `.dat` entry of their own — each base type names its
    // array's OID, and the array's name is the base's with a leading underscore.
    // `pg_proc` argument lists reference those names (`_cstring`, `_text`), so
    // the map has to carry them.
    for e in &type_entries {
        let (Some(name), array_oid) = (get(e, "typname"), oid_field(e, "array_type_oid")) else {
            continue;
        };
        if array_oid != 0 {
            name_to_oid.insert(format!("_{name}"), array_oid);
        }
    }
    let procs = ProcIndex::build(&proc_entries);

    // Every function OID the other catalogs point at. Emitting `pg_proc` rows
    // for exactly these keeps each row justified by an inbound reference and
    // keeps the catalog from advertising thousands of functions this build
    // cannot run.
    let mut referenced: Vec<u32> = Vec::new();
    for e in &type_entries {
        for key in TYPE_REGPROC_COLUMNS {
            if let Some(oid) = procs.by_name(str_field(e, key, "-")) {
                referenced.push(oid);
            }
        }
    }
    for e in &cast_entries {
        if let Some(oid) = procs.by_signature(str_field(e, "castfunc", "0")) {
            referenced.push(oid);
        }
    }
    for handler in AM_HANDLERS {
        if let Some(oid) = procs.by_name(handler) {
            referenced.push(oid);
        }
    }
    // The derived array rows name these rather than inheriting the element's,
    // so they are referenced by rows no `.dat` entry spells out.
    for name in ARRAY_ROW_PROCS {
        if let Some(oid) = procs.by_name(name) {
            referenced.push(oid);
        }
    }
    referenced.sort_unstable();
    referenced.dedup();

    std::fs::write(
        out_dir.join("pg_type_rows.rs"),
        gen_pg_type(&type_entries, &name_to_oid, &procs),
    )?;
    std::fs::write(
        out_dir.join("pg_cast_rows.rs"),
        gen_pg_cast(&cast_entries, &name_to_oid, &procs),
    )?;
    std::fs::write(
        out_dir.join("pg_proc_rows.rs"),
        gen_pg_proc(&proc_entries, &name_to_oid, &referenced),
    )?;
    Ok(())
}

/// The `pg_type` columns that hold a bare `regproc` name.
const TYPE_REGPROC_COLUMNS: &[&str] = &[
    "typinput",
    "typoutput",
    "typreceive",
    "typsend",
    "typmodin",
    "typmodout",
    "typanalyze",
    "typsubscript",
];

/// The `regproc` names [`array_row_for`] gives every derived array row. They
/// belong to the array family rather than to the element, so nothing in
/// `pg_type.dat` necessarily references them.
const ARRAY_ROW_PROCS: &[&str] = &["array_in", "array_out", "array_recv", "array_send"];

/// The `pg_am.amhandler` names `catalogs::am::pg_am_rows` publishes that upstream
/// also has. crabgresql's own access methods (`parquet`, `buffer`) have no
/// upstream entry to resolve against; `oids::OWN_AM_HANDLERS` gives those
/// two `pg_proc` rows of their own.
const AM_HANDLERS: &[&str] = &[
    "heap_tableam_handler",
    "bthandler",
    "hashhandler",
    "gisthandler",
    "ginhandler",
    "brinhandler",
    "spghandler",
];

/// `pg_proc.dat` indexed the two ways the other catalogs reference a function:
/// by bare name (`regproc`, as `pg_type.typinput` spells it) and by full
/// signature (`regprocedure`, as `pg_cast.castfunc` spells it).
struct ProcIndex<'a> {
    /// Bare `proname` -> OID. A name carried by more than one entry is dropped:
    /// upstream's own `regproc` references must be unambiguous, so a duplicate
    /// here would mean the reference cannot be resolved by name either.
    by_name: HashMap<&'a str, Option<u32>>,
    /// `name(argtype,argtype)` -> OID, the spelling `pg_cast.dat` uses.
    by_signature: HashMap<String, u32>,
}

impl<'a> ProcIndex<'a> {
    fn build(entries: &'a [Entry]) -> Self {
        let mut by_name: HashMap<&'a str, Option<u32>> = HashMap::new();
        let mut by_signature = HashMap::new();
        for e in entries {
            let (Some(name), oid) = (get(e, "proname"), oid_field(e, "oid")) else {
                continue;
            };
            by_name
                .entry(name)
                .and_modify(|slot| *slot = None)
                .or_insert(Some(oid));
            by_signature.insert(signature(name, str_field(e, "proargtypes", "")), oid);
        }
        Self {
            by_name,
            by_signature,
        }
    }

    /// The OID a bare `regproc` reference names. `-` (the `.dat` spelling of
    /// "none") and an ambiguous name both resolve to nothing.
    fn by_name(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied().flatten()
    }

    /// The OID a `regprocedure` reference names. `0` is upstream's spelling of
    /// a binary-coercible cast, which has no function.
    fn by_signature(&self, sig: &str) -> Option<u32> {
        self.by_signature.get(sig).copied()
    }
}

/// A `regprocedure` spelling: `name(arg,arg)`, or `name()` for no arguments.
/// `pg_proc.dat` writes the argument types space-separated, `pg_cast.dat`
/// comma-separated.
fn signature(name: &str, argtypes: &str) -> String {
    let args: Vec<&str> = argtypes.split_whitespace().collect();
    format!("{name}({})", args.join(","))
}

fn read_dat(dir: &std::path::Path, file: &str) -> std::io::Result<Vec<Entry>> {
    let path = dir.join(file);
    println!("cargo:rerun-if-changed={}", path.display());
    let src = std::fs::read_to_string(&path)?;
    Ok(parse_dat(&src))
}

/// One `.dat` entry: its `key => value` pairs with quotes stripped.
type Entry = HashMap<String, String>;

/// Parse a PostgreSQL `.dat` file (a Perl array of `{ key => 'value', ... }`
/// hashes) into a list of key→value maps. Comments (`#` to end of line, outside
/// quotes) and the surrounding `[` `]` are ignored. Single-quoted values may
/// contain `\'`/`\\` escapes.
fn parse_dat(src: &str) -> Vec<Entry> {
    let bytes = src.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    let mut entries = Vec::new();

    while i < n {
        match bytes[i] {
            b'#' => {
                // Comment to end of line.
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                let (entry, next) = parse_entry(bytes, i + 1);
                entries.push(entry);
                i = next;
            }
            _ => i += 1,
        }
    }
    entries
}

/// Parse one `{ ... }` body starting at `start` (just past the `{`), returning
/// the entry and the index just past the closing `}`.
fn parse_entry(bytes: &[u8], start: usize) -> (Entry, usize) {
    let mut entry = Entry::new();
    let mut i = start;
    let n = bytes.len();
    loop {
        i = skip_ws_and_commas(bytes, i);
        if i >= n || bytes[i] == b'}' {
            return (entry, i + 1);
        }
        // A key: identifier chars up to whitespace or '='.
        let key_start = i;
        while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let key = String::from_utf8_lossy(&bytes[key_start..i]).into_owned();
        // '=>'
        i = skip_ws_and_commas(bytes, i);
        if i + 1 < n && bytes[i] == b'=' && bytes[i + 1] == b'>' {
            i += 2;
        }
        i = skip_ws_and_commas(bytes, i);
        // Value: quoted string or bareword (e.g. `_null_`).
        let value = if i < n && bytes[i] == b'\'' {
            let (v, next) = parse_quoted(bytes, i + 1);
            i = next;
            v
        } else {
            let vs = i;
            while i < n && bytes[i] != b',' && bytes[i] != b'}' && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            String::from_utf8_lossy(&bytes[vs..i]).into_owned()
        };
        entry.insert(key, value);
    }
}

fn skip_ws_and_commas(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
        i += 1;
    }
    i
}

/// Parse a single-quoted value starting past the opening quote; returns the
/// unescaped content and the index past the closing quote.
fn parse_quoted(bytes: &[u8], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut i = start;
    let n = bytes.len();
    while i < n {
        match bytes[i] {
            b'\\' if i + 1 < n => {
                out.push(bytes[i + 1] as char);
                i += 2;
            }
            b'\'' => return (out, i + 1),
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    (out, i)
}

/// Symbolic `typlen` constants used in `pg_type.dat`, which PostgreSQL's own
/// catalog generator substitutes before the values reach `pg_type`. We fix the
/// 64-bit answers, matching the `server_version` we report.
fn resolve_typlen(s: &str) -> i16 {
    match s {
        "NAMEDATALEN" => 64,
        "SIZEOF_POINTER" => 8,
        other => other
            .parse()
            .unwrap_or_else(|_| panic!("unexpected typlen {other:?}")),
    }
}

/// `typalign`, likewise: `pg_type.dat` spells two entries' alignment as a
/// symbol, and PostgreSQL serves the substituted single character. On the
/// 64-bit target [`resolve_typlen`] already assumes, a pointer is 8 bytes and so
/// aligns like a double.
fn resolve_typalign(s: &str) -> &str {
    match s {
        "ALIGNOF_POINTER" => "d",
        // The whole vocabulary of the column, spelled out so a typo in a
        // re-vendored file fails the build instead of reaching `pg_type`.
        "c" | "s" | "i" | "d" => s,
        other => panic!("unexpected typalign {other:?}"),
    }
}

fn get<'a>(e: &'a Entry, key: &str) -> Option<&'a str> {
    e.get(key).map(String::as_str).filter(|v| *v != "_null_")
}

fn oid_field(e: &Entry, key: &str) -> u32 {
    match get(e, key) {
        Some(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("bad oid {key}={value:?}")),
        None => 0,
    }
}

fn bool_field(e: &Entry, key: &str, default: bool) -> bool {
    match get(e, key) {
        Some("t") => true,
        Some("f") => false,
        None => default,
        Some(other) => panic!("bad bool {key}={other:?}"),
    }
}

fn str_field<'a>(e: &'a Entry, key: &str, default: &'a str) -> &'a str {
    get(e, key).unwrap_or(default)
}

/// One emitted `PgTypeRow` literal. Both the base rows read from `pg_type.dat`
/// and the array rows derived from them go through this, so the two can never
/// drift in column order or in the fields this build hardcodes.
struct TypeRow<'a> {
    oid: u32,
    typname: String,
    typlen: i16,
    typbyval: bool,
    typtype: &'a str,
    typcategory: &'a str,
    typispreferred: bool,
    typdelim: &'a str,
    typsubscript: String,
    typelem: u32,
    typarray: u32,
    typinput: String,
    typoutput: String,
    typreceive: String,
    typsend: String,
    typmodin: String,
    typmodout: String,
    typanalyze: String,
    typalign: &'a str,
    typstorage: &'a str,
    /// A Rust *expression* for the collation OID, not the OID itself: the
    /// generated file is `include!`d into the crate, so a named constant reads
    /// better than the bare number. Emitted with `{}`, unlike its string
    /// neighbours — `{:?}` here would quote it into a `u32` field.
    typcollation: &'a str,
}

impl TypeRow<'_> {
    fn emit(&self) -> String {
        let TypeRow {
            oid,
            typname,
            typlen,
            typbyval,
            typtype,
            typcategory,
            typispreferred,
            typdelim,
            typsubscript,
            typelem,
            typarray,
            typinput,
            typoutput,
            typreceive,
            typsend,
            typmodin,
            typmodout,
            typanalyze,
            typalign,
            typstorage,
            typcollation,
        } = self;
        format!(
            "    PgTypeRow {{ oid: {oid}, typname: {typname:?}, typnamespace: 11, \
typowner: crate::oids::BOOTSTRAP_ROLE_OID, typlen: {typlen}, typbyval: {typbyval}, \
typtype: {typtype:?}, \
typcategory: {typcategory:?}, typispreferred: {typispreferred}, typisdefined: true, \
typdelim: {typdelim:?}, typrelid: {typrelid}, typsubscript: {typsubscript}, \
typelem: {typelem}, typarray: {typarray}, \
typinput: {typinput}, typoutput: {typoutput}, typreceive: {typreceive}, \
typsend: {typsend}, typmodin: {typmodin}, typmodout: {typmodout}, \
typanalyze: {typanalyze}, typalign: {typalign:?}, typstorage: {typstorage:?}, \
typcollation: {typcollation} }},\n",
            // typrelid references a catalog relation's pg_class OID, which we do
            // not codegen yet — 0 until pg_class OIDs are available (only the
            // handful of catalog composite types carry a nonzero value).
            typrelid = 0,
        )
    }
}

/// `typcollation` as `pg_type.dat` writes it: the *name* of the collation a
/// value of the type sorts under, or absent when the type is not collatable.
/// Only two names appear, and both have a constant in `crabgresql-types`.
fn resolve_typcollation(e: &Entry) -> &'static str {
    match get(e, "typcollation") {
        None => "0",
        Some("C") => "crabgresql_types::collation::C_COLLATION_OID",
        Some("default") => "crabgresql_types::collation::DEFAULT_COLLATION_OID",
        Some(other) => panic!("unexpected typcollation {other:?}"),
    }
}

/// A `regproc` column as the `ProcRef` expression the generated file carries:
/// the name as written, plus the OID it resolves to. `-` is the catalog's
/// spelling of "no function" and resolves to 0, which prints back as `-`.
fn proc_ref(procs: &ProcIndex<'_>, name: &str) -> String {
    let oid = procs.by_name(name).unwrap_or(0);
    format!("ProcRef {{ oid: {oid}, name: {name:?} }}")
}

/// Emit `PG_TYPE_ROWS: &[PgTypeRow]` from `pg_type.dat`. Each base entry that
/// names an `array_type_oid` is followed by the row for its array type (see
/// [`array_row_for`]); the few array types the `.dat` spells out itself
/// (`_record`) come through as ordinary entries. Omitted columns fall back to
/// PostgreSQL's `pg_type` BKI defaults.
fn gen_pg_type(
    entries: &[Entry],
    name_to_oid: &HashMap<String, u32>,
    procs: &ProcIndex<'_>,
) -> String {
    let regproc = |e: &Entry, key: &str| proc_ref(procs, str_field(e, key, "-"));
    // Every OID and name the file itself claims, so a derived array row that
    // collides with a real type is a build failure rather than a duplicate row.
    // Both maps are filled before the loop: an entry that collides with a
    // derived row can appear anywhere in the file, above or below its element.
    let mut used_oids: HashMap<u32, String> = entries
        .iter()
        .map(|e| (oid_field(e, "oid"), str_field(e, "typname", "").to_string()))
        .collect();
    let mut used_names: HashMap<String, u32> = entries
        .iter()
        .map(|e| (str_field(e, "typname", "").to_string(), oid_field(e, "oid")))
        .collect();
    let mut out = String::new();
    out.push_str("// @generated by build.rs from vendor/postgres/catalog/pg_type.dat\n");
    out.push_str("pub static PG_TYPE_ROWS: &[PgTypeRow] = &[\n");
    for e in entries {
        let oid = oid_field(e, "oid");
        let typname = str_field(e, "typname", "");
        assert!(
            oid != 0 && !typname.is_empty(),
            "pg_type entry missing oid/typname"
        );
        // typarray is usually given as `array_type_oid` (the array type is
        // derived); the entries whose array cannot be derived instead name it
        // outright, and that name resolves like any other reference.
        let array_oid = oid_field(e, "array_type_oid");
        let typarray = match array_oid {
            0 => get(e, "typarray")
                .map(|n| name_to_oid.get(n).copied().unwrap_or(0))
                .unwrap_or(0),
            autogen => autogen,
        };
        let base = TypeRow {
            oid,
            typname: typname.to_string(),
            typlen: resolve_typlen(str_field(e, "typlen", "0")),
            typbyval: bool_field(e, "typbyval", false),
            typtype: str_field(e, "typtype", "b"),
            typcategory: str_field(e, "typcategory", "X"),
            typispreferred: bool_field(e, "typispreferred", false),
            typdelim: str_field(e, "typdelim", ","),
            typsubscript: regproc(e, "typsubscript"),
            // typelem: element type by name (0 when the type is not an array/vector).
            typelem: get(e, "typelem")
                .map(|n| name_to_oid.get(n).copied().unwrap_or(0))
                .unwrap_or(0),
            typarray,
            typinput: regproc(e, "typinput"),
            typoutput: regproc(e, "typoutput"),
            typreceive: regproc(e, "typreceive"),
            typsend: regproc(e, "typsend"),
            typmodin: regproc(e, "typmodin"),
            typmodout: regproc(e, "typmodout"),
            typanalyze: regproc(e, "typanalyze"),
            typalign: resolve_typalign(str_field(e, "typalign", "i")),
            typstorage: str_field(e, "typstorage", "p"),
            typcollation: resolve_typcollation(e),
        };
        out.push_str(&base.emit());

        if array_oid == 0 {
            continue;
        }
        let array = array_row_for(&base, array_oid, procs);
        if let Some(other) = used_oids.insert(array_oid, array.typname.clone()) {
            panic!(
                "array type OID {array_oid} for {typname:?} collides with existing type {other:?}"
            );
        }
        if let Some(other) = used_names.insert(array.typname.clone(), array_oid) {
            panic!(
                "array type name {:?} for {typname:?} collides with existing type oid {other}",
                array.typname
            );
        }
        out.push_str(&array.emit());
    }
    out.push_str("];\n");
    let _ = writeln!(out); // trailing newline
    out
}

/// The array type PostgreSQL serves for an entry carrying `array_type_oid`. A
/// varlena of the element type with the array I/O functions: `typname` gets the
/// conventional `_` prefix, `typcategory` is `A`, and storage is extended.
/// `typdelim` and `typcollation` are inherited (`box` separates with `;`, so
/// `_box` does too; `_text` sorts under the element's collation), but `typalign`
/// is *not* — it widens to `i` for everything narrower, since the array header
/// is int-aligned. The `regproc` columns are the array family's own, not the
/// element's: `array_in`/`array_out`/`array_recv`/`array_send`,
/// `array_subscript_handler` and `array_typanalyze`. Every value here is pinned
/// against a live `pg_type` by `array_rows_are_derived_from_their_element`.
fn array_row_for<'a>(base: &TypeRow<'a>, oid: u32, procs: &ProcIndex<'_>) -> TypeRow<'a> {
    TypeRow {
        oid,
        typname: format!("_{}", base.typname),
        typlen: -1,
        typbyval: false,
        typtype: "b",
        typcategory: "A",
        typispreferred: false,
        typdelim: base.typdelim,
        typsubscript: proc_ref(procs, "array_subscript_handler"),
        typelem: base.oid,
        // An array of arrays is not a type of its own here, as upstream.
        typarray: 0,
        typinput: proc_ref(procs, "array_in"),
        typoutput: proc_ref(procs, "array_out"),
        typreceive: proc_ref(procs, "array_recv"),
        typsend: proc_ref(procs, "array_send"),
        // An array takes no typmod and uses the array-wide statistics routine.
        typmodin: proc_ref(procs, "-"),
        typmodout: proc_ref(procs, "-"),
        typanalyze: proc_ref(procs, "array_typanalyze"),
        typalign: if base.typalign == "d" { "d" } else { "i" },
        typstorage: "x",
        typcollation: base.typcollation,
    }
}

/// Emit `PG_CAST_ROWS: &[PgCastRow]` from `pg_cast.dat`. `castsource`/
/// `casttarget` are written as type names (resolved here against `pg_type`);
/// a cast whose source or target is a type crabgresql does not expose is
/// skipped, so the emitted `pg_cast` stays consistent with the exposed
/// `pg_type`. OIDs are synthetic (upstream assigns none). `castfunc` is a
/// resolved `pg_proc` OID, as PostgreSQL declares the column — `0` for a
/// binary-coercible cast, which needs no function.
fn gen_pg_cast(
    entries: &[Entry],
    name_to_oid: &HashMap<String, u32>,
    procs: &ProcIndex<'_>,
) -> String {
    // Casts have no manual OIDs upstream; assign stable synthetic ones above the
    // built-in floor so they never collide with a real object OID.
    const FIRST_CAST_OID: u32 = 10000;
    let mut out = String::new();
    out.push_str("// @generated by build.rs from vendor/postgres/catalog/pg_cast.dat\n");
    out.push_str("pub static PG_CAST_ROWS: &[PgCastRow] = &[\n");
    let mut next_oid = FIRST_CAST_OID;
    for e in entries {
        let (Some(source), Some(target)) = (
            get(e, "castsource").and_then(|n| name_to_oid.get(n).copied()),
            get(e, "casttarget").and_then(|n| name_to_oid.get(n).copied()),
        ) else {
            continue; // references a type we do not expose
        };
        let row = format!(
            "    PgCastRow {{ oid: {oid}, castsource: {source}, casttarget: {target}, \
castfunc: {castfunc}, castcontext: {castcontext:?}, castmethod: {castmethod:?} }},\n",
            oid = next_oid,
            castfunc = procs
                .by_signature(str_field(e, "castfunc", "0"))
                .unwrap_or(0),
            castcontext = str_field(e, "castcontext", "e"),
            castmethod = str_field(e, "castmethod", "f"),
        );
        next_oid += 1;
        out.push_str(&row);
    }
    out.push_str("];\n");
    let _ = writeln!(out);
    out
}

/// Emit `PG_PROC_ROWS: &[PgProcRow]` for the functions listed in `referenced` —
/// the ones `pg_type`, `pg_cast` and `pg_am` point at. The rest of
/// `pg_proc.dat` is deliberately left out: a `pg_proc` row is a claim that the
/// function exists, and this build implements its SQL surface from its own
/// registry, not from upstream's list. Every row emitted here is justified by
/// an inbound reference that would otherwise dangle.
///
/// Omitted fields fall back to PostgreSQL's `pg_proc` BKI defaults (authored
/// from the public catalog docs and checked against a running 18.4).
fn gen_pg_proc(
    entries: &[Entry],
    name_to_oid: &HashMap<String, u32>,
    referenced: &[u32],
) -> String {
    let type_oid = |name: &str| {
        name_to_oid
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("pg_proc references type {name:?}, which pg_type.dat lacks"))
    };
    let mut out = String::new();
    out.push_str("// @generated by build.rs from vendor/postgres/catalog/pg_proc.dat\n");
    out.push_str("pub static PG_PROC_ROWS: &[PgProcRow] = &[\n");
    let mut rows: Vec<(u32, String)> = Vec::new();
    for e in entries {
        let oid = oid_field(e, "oid");
        if referenced.binary_search(&oid).is_err() {
            continue;
        }
        let proname = str_field(e, "proname", "");
        // The emitted subset is I/O, cast and handler functions, none of which
        // take OUT parameters or defaults. Anything else would need those
        // columns filled rather than left NULL, so refuse rather than lie.
        for unsupported in [
            "proallargtypes",
            "proargmodes",
            "proargnames",
            "proargdefaults",
        ] {
            assert!(
                get(e, unsupported).is_none(),
                "pg_proc entry {proname} carries {unsupported}, which codegen does not emit"
            );
        }
        let argtypes: Vec<u32> = str_field(e, "proargtypes", "")
            .split_whitespace()
            .map(type_oid)
            .collect();
        let retset = bool_field(e, "proretset", false);
        let args = argtypes
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let row = format!(
            "    PgProcRow {{ oid: {oid}, proname: {proname:?}, prolang: {prolang}, \
procost: {procost:?}, prorows: {prorows:?}, provariadic: {provariadic}, \
prosupport: {prosupport}, prokind: {prokind:?}, prosecdef: false, \
proleakproof: {proleakproof}, proisstrict: {proisstrict}, proretset: {retset}, \
provolatile: {provolatile:?}, proparallel: {proparallel:?}, pronargs: {pronargs}, \
prorettype: {prorettype}, proargtypes: &[{args}], prosrc: {prosrc:?} }},\n",
            prolang = match str_field(e, "prolang", "internal") {
                "c" => 13,
                "sql" => 14,
                other => {
                    assert_eq!(other, "internal", "unknown prolang {other:?}");
                    12
                }
            },
            procost = str_field(e, "procost", "1")
                .parse::<f32>()
                .unwrap_or_else(|_| panic!("bad procost on {proname}")),
            // A set-returning function with no explicit estimate reports
            // PostgreSQL's 1000-row planner default; a scalar one reports 0.
            prorows = match get(e, "prorows") {
                Some(rows) => rows
                    .parse::<f32>()
                    .unwrap_or_else(|_| panic!("bad prorows on {proname}")),
                None if retset => 1000.0,
                None => 0.0,
            },
            provariadic = get(e, "provariadic").map_or(0, type_oid),
            // A planner support routine is itself a `pg_proc` row this codegen
            // does not emit, so pointing at one would dangle. Report none —
            // which is also true of what the planner here actually consults.
            prosupport = "ProcRef { oid: 0, name: \"-\" }",
            prokind = str_field(e, "prokind", "f"),
            proleakproof = bool_field(e, "proleakproof", false),
            proisstrict = bool_field(e, "proisstrict", true),
            provolatile = str_field(e, "provolatile", "i"),
            proparallel = str_field(e, "proparallel", "s"),
            pronargs = argtypes.len(),
            prorettype = type_oid(str_field(e, "prorettype", "")),
            prosrc = str_field(e, "prosrc", ""),
        );
        rows.push((oid, row));
    }
    rows.sort_by_key(|(oid, _)| *oid);
    for (_, row) in &rows {
        out.push_str(row);
    }
    out.push_str("];\n");
    let _ = writeln!(out);
    out
}
