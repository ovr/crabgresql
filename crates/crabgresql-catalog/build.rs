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

    // Base type name -> OID, used to resolve name references in pg_type
    // (`typelem`) and pg_cast (`castsource`/`casttarget`).
    let name_to_oid: HashMap<&str, u32> = type_entries
        .iter()
        .filter_map(|e| Some((get(e, "typname")?, oid_field(e, "oid"))))
        .collect();

    std::fs::write(
        out_dir.join("pg_type_rows.rs"),
        gen_pg_type(&type_entries, &name_to_oid),
    )?;
    std::fs::write(
        out_dir.join("pg_cast_rows.rs"),
        gen_pg_cast(&cast_entries, &name_to_oid),
    )?;
    Ok(())
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

/// Symbolic `typlen` constants used in `pg_type.dat` (resolved by `genbki.pl`
/// upstream). We fix the 64-bit values, matching the `server_version` we report.
fn resolve_typlen(s: &str) -> i16 {
    match s {
        "NAMEDATALEN" => 64,
        "SIZEOF_POINTER" => 8,
        other => other
            .parse()
            .unwrap_or_else(|_| panic!("unexpected typlen {other:?}")),
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
    typelem: u32,
    typarray: u32,
    typinput: &'a str,
    typoutput: &'a str,
    typreceive: &'a str,
    typsend: &'a str,
    typalign: &'a str,
    typstorage: &'a str,
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
            typelem,
            typarray,
            typinput,
            typoutput,
            typreceive,
            typsend,
            typalign,
            typstorage,
        } = self;
        format!(
            "    PgTypeRow {{ oid: {oid}, typname: {typname:?}, typnamespace: 11, \
typowner: crate::schema::BOOTSTRAP_ROLE_OID, typlen: {typlen}, typbyval: {typbyval}, \
typtype: {typtype:?}, \
typcategory: {typcategory:?}, typispreferred: {typispreferred}, typisdefined: true, \
typdelim: {typdelim:?}, typrelid: {typrelid}, typelem: {typelem}, typarray: {typarray}, \
typinput: {typinput:?}, typoutput: {typoutput:?}, typreceive: {typreceive:?}, \
typsend: {typsend:?}, typalign: {typalign:?}, typstorage: {typstorage:?} }},\n",
            // typrelid references a catalog relation's pg_class OID, which we do
            // not codegen yet — 0 until pg_class OIDs are available (only the
            // handful of catalog composite types carry a nonzero value).
            typrelid = 0,
        )
    }
}

/// Emit `PG_TYPE_ROWS: &[PgTypeRow]` from `pg_type.dat`. Each base entry that
/// names an `array_type_oid` is followed by the row for its array type, derived
/// here the way `genbki.pl` derives it upstream (see [`array_row_for`]); the few
/// array types the `.dat` spells out itself (`_record`) come through as ordinary
/// entries. Omitted columns fall back to PostgreSQL's `pg_type` BKI defaults.
fn gen_pg_type(entries: &[Entry], name_to_oid: &HashMap<&str, u32>) -> String {
    // Every OID the file itself claims, so a derived array OID that collides
    // with a real type is a build failure rather than a duplicate row.
    let mut used: HashMap<u32, String> = entries
        .iter()
        .map(|e| (oid_field(e, "oid"), str_field(e, "typname", "").to_string()))
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
        // typarray is usually given as `array_type_oid` (genbki autogenerates
        // the type); the entries whose array cannot be autogenerated instead
        // name it outright, and that name resolves like any other reference.
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
            // typelem: element type by name (0 when the type is not an array/vector).
            typelem: get(e, "typelem")
                .map(|n| name_to_oid.get(n).copied().unwrap_or(0))
                .unwrap_or(0),
            typarray,
            typinput: str_field(e, "typinput", "-"),
            typoutput: str_field(e, "typoutput", "-"),
            typreceive: str_field(e, "typreceive", "-"),
            typsend: str_field(e, "typsend", "-"),
            typalign: str_field(e, "typalign", "i"),
            typstorage: str_field(e, "typstorage", "p"),
        };
        out.push_str(&base.emit());

        if array_oid == 0 {
            continue;
        }
        let array = array_row_for(&base, array_oid);
        if let Some(other) = used.insert(array_oid, array.typname.clone()) {
            panic!(
                "array type OID {array_oid} for {typname:?} collides with existing type {other:?}"
            );
        }
        out.push_str(&array.emit());
    }
    out.push_str("];\n");
    let _ = writeln!(out); // trailing newline
    out
}

/// The array type PostgreSQL autogenerates for `base`, per the rules `genbki.pl`
/// applies to an entry carrying `array_type_oid`. A varlena of the element type
/// with the array I/O functions: `typname` gets the conventional `_` prefix,
/// `typcategory` is `A`, and storage is extended. `typdelim` is inherited (`box`
/// separates with `;`, so `_box` does too), but `typalign` is *not* — it widens
/// to `i` for everything narrower, since the array header is int-aligned.
fn array_row_for<'a>(base: &TypeRow<'a>, oid: u32) -> TypeRow<'a> {
    TypeRow {
        oid,
        typname: format!("_{}", base.typname),
        typlen: -1,
        typbyval: false,
        typtype: "b",
        typcategory: "A",
        typispreferred: false,
        typdelim: base.typdelim,
        typelem: base.oid,
        // An array of arrays is not a type of its own here, as upstream.
        typarray: 0,
        typinput: "array_in",
        typoutput: "array_out",
        typreceive: "array_recv",
        typsend: "array_send",
        typalign: if base.typalign == "d" { "d" } else { "i" },
        typstorage: "x",
    }
}

/// Emit `PG_CAST_ROWS: &[PgCastRow]` from `pg_cast.dat`. `castsource`/
/// `casttarget` are written as type names (resolved here against `pg_type`);
/// a cast whose source or target is a type crabgresql does not expose is
/// skipped, so the emitted `pg_cast` stays consistent with the exposed
/// `pg_type`. OIDs are synthetic (upstream assigns none). `castfunc` is kept as
/// the `.dat` text (a `regprocedure` reference) rather than a resolved OID.
fn gen_pg_cast(entries: &[Entry], name_to_oid: &HashMap<&str, u32>) -> String {
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
castfunc: {castfunc:?}, castcontext: {castcontext:?}, castmethod: {castmethod:?} }},\n",
            oid = next_oid,
            castfunc = str_field(e, "castfunc", "-"),
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
