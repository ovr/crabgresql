//! Build-time codegen for `pg_catalog`'s built-in rows.
//!
//! Reads PostgreSQL's vendored catalog *data* files
//! (`vendor/postgres/catalog/*.dat`) and emits the typed row arrays
//! `crabgresql-catalog` includes at compile time. The `.dat` format is a Perl
//! array-of-hashes; [`dat`] reads that DATA only — it is an original scanner,
//! NOT a port of PostgreSQL's `Catalog.pm`. Fields a `.dat` entry omits are
//! filled from the per-catalog defaults in the emitters (authored from the
//! public catalog docs), so codegen never reads PostgreSQL's C headers.
//!
//! This lives in a library rather than in `crabgresql-catalog/build.rs` so the
//! parser, the symbol resolution and the emitters can be unit-tested; the
//! build script is the thin wrapper that calls [`generate`].
//!
//! Codegen runs in the two phases [`symbols`] describes: every catalog defines
//! its symbols, and only then does any catalog resolve a reference. That is
//! what lets `pg_type.typinput` point at a `pg_proc` row whose `prorettype`
//! points back at `pg_type`.
//!
//! See `docs/ARCHITECTURE.md` §7 and `AGENTS.md`: vendoring catalog `.dat` data
//! and generating from it is the sanctioned path; attribution is in `NOTICE`.

pub mod dat;
mod pg_aggregate;
mod pg_amop;
mod pg_amproc;
mod pg_cast;
mod pg_description;
mod pg_opclass;
mod pg_operator;
mod pg_opfamily;
mod pg_proc;
mod pg_ts;
mod pg_type;
pub mod symbols;

use std::path::Path;

use dat::read_dat;
use symbols::SymbolKind::Proc;
use symbols::SymbolTable;

/// The functions the catalogs `crabgresql-catalog` builds **by hand**
/// reference. Their references cannot be recorded the way a generated
/// catalog's are — no `.dat` spells them out — so they are declared here, and
/// the census emits their `pg_proc` rows like any other.
///
/// Two catalogs are on this list:
///
///   - `catalogs::am::pg_am_rows` publishes upstream's access methods;
///     crabgresql's own (`parquet`, `buffer`) have no upstream entry to resolve
///     against, so `oids::OWN_AM_HANDLERS` gives those two `pg_proc` rows of
///     their own.
///   - `catalogs::types::pg_type_user_rows` gives every `CREATE TYPE ... AS
///     ENUM` the four enum I/O functions, exactly as upstream does.
///     `pg_type.dat` names only `anyenum_in`/`anyenum_out`, so without this the
///     columns rendered `-` and the type claimed to have no input function at
///     all.
const HANDWRITTEN_CATALOG_PROCS: &[&str] = &[
    "heap_tableam_handler",
    "bthandler",
    "hashhandler",
    "gisthandler",
    "ginhandler",
    "brinhandler",
    "spghandler",
    "enum_in",
    "enum_out",
    "enum_recv",
    "enum_send",
];

/// Read the vendored `.dat` files in `catalog_dir` and write the generated row
/// arrays into `out_dir` (cargo's `OUT_DIR`).
///
/// `pg_proc` is emitted last of the row catalogs on purpose: it emits exactly
/// the functions the other catalogs turned out to reference, and
/// [`SymbolTable::references`] enforces that ordering. `pg_description` comes
/// after even that, because it filters on the same census.
///
/// `pg_am.dat`, `pg_language.dat` and `pg_namespace.dat` are read for their
/// `descr` fields alone — the rows of those three catalogs are hand-written in
/// `crabgresql-catalog`.
pub fn generate(catalog_dir: &Path, out_dir: &Path) -> std::io::Result<()> {
    let type_entries = read_dat(catalog_dir, "pg_type.dat")?;
    let cast_entries = read_dat(catalog_dir, "pg_cast.dat")?;
    let proc_entries = read_dat(catalog_dir, "pg_proc.dat")?;
    let opfamily_entries = read_dat(catalog_dir, "pg_opfamily.dat")?;
    let opclass_entries = read_dat(catalog_dir, "pg_opclass.dat")?;
    let operator_entries = read_dat(catalog_dir, "pg_operator.dat")?;
    let aggregate_entries = read_dat(catalog_dir, "pg_aggregate.dat")?;
    let amop_entries = read_dat(catalog_dir, "pg_amop.dat")?;
    let amproc_entries = read_dat(catalog_dir, "pg_amproc.dat")?;
    let ts_parser_entries = read_dat(catalog_dir, "pg_ts_parser.dat")?;
    let ts_template_entries = read_dat(catalog_dir, "pg_ts_template.dat")?;
    let ts_dict_entries = read_dat(catalog_dir, "pg_ts_dict.dat")?;
    let ts_config_entries = read_dat(catalog_dir, "pg_ts_config.dat")?;
    let ts_config_map_entries = read_dat(catalog_dir, "pg_ts_config_map.dat")?;
    let am_entries = read_dat(catalog_dir, "pg_am.dat")?;
    let language_entries = read_dat(catalog_dir, "pg_language.dat")?;
    let namespace_entries = read_dat(catalog_dir, "pg_namespace.dat")?;

    // Phase one: define.
    let mut symbols = SymbolTable::default();
    pg_type::define_symbols(&type_entries, &mut symbols);
    pg_proc::define_symbols(&proc_entries, &mut symbols);
    pg_opfamily::define_symbols(&opfamily_entries, &mut symbols);
    pg_operator::define_symbols(&operator_entries, &mut symbols);

    // Phase two: resolve and emit.
    std::fs::write(
        out_dir.join("pg_type_rows.rs"),
        pg_type::emit(&type_entries, &symbols),
    )?;
    std::fs::write(
        out_dir.join("pg_cast_rows.rs"),
        pg_cast::emit(&cast_entries, &symbols),
    )?;
    std::fs::write(
        out_dir.join("pg_opfamily_rows.rs"),
        pg_opfamily::emit(&opfamily_entries, &symbols),
    )?;
    std::fs::write(
        out_dir.join("pg_opclass_rows.rs"),
        pg_opclass::emit(&opclass_entries, &symbols),
    )?;
    std::fs::write(
        out_dir.join("pg_operator_rows.rs"),
        pg_operator::emit(&operator_entries, &symbols),
    )?;
    std::fs::write(
        out_dir.join("pg_aggregate_rows.rs"),
        pg_aggregate::emit(&aggregate_entries, &symbols),
    )?;
    std::fs::write(
        out_dir.join("pg_amop_rows.rs"),
        pg_amop::emit(&amop_entries, &symbols),
    )?;
    std::fs::write(
        out_dir.join("pg_amproc_rows.rs"),
        pg_amproc::emit(&amproc_entries, &symbols),
    )?;
    let ts = pg_ts::emit(
        &ts_parser_entries,
        &ts_template_entries,
        &ts_dict_entries,
        &ts_config_entries,
        &ts_config_map_entries,
        &symbols,
    );
    std::fs::write(out_dir.join("pg_ts_parser_rows.rs"), ts.parsers)?;
    std::fs::write(out_dir.join("pg_ts_template_rows.rs"), ts.templates)?;
    std::fs::write(out_dir.join("pg_ts_dict_rows.rs"), ts.dicts)?;
    std::fs::write(out_dir.join("pg_ts_config_rows.rs"), ts.configs)?;
    std::fs::write(out_dir.join("pg_ts_config_map_rows.rs"), ts.config_map)?;
    for name in HANDWRITTEN_CATALOG_PROCS {
        assert!(
            symbols.resolve_name(Proc, name).is_some(),
            "{name} is referenced by a hand-written catalog but pg_proc.dat \
             defines no unique entry for it"
        );
    }
    std::fs::write(
        out_dir.join("pg_proc_rows.rs"),
        pg_proc::emit(&proc_entries, &symbols),
    )?;
    // Re-reading a sealed census is safe: the seal forbids resolving a *new*
    // OID afterwards, not reading the same list twice.
    let referenced_procs = symbols.references(Proc);
    std::fs::write(
        out_dir.join("pg_description_rows.rs"),
        pg_description::emit(&[
            pg_description::Source {
                catalog: "pg_type",
                entries: &type_entries,
                keep: None,
            },
            pg_description::Source {
                catalog: "pg_proc",
                entries: &proc_entries,
                keep: Some(&referenced_procs),
            },
            pg_description::Source {
                catalog: "pg_operator",
                entries: &operator_entries,
                keep: None,
            },
            pg_description::Source {
                catalog: "pg_ts_parser",
                entries: &ts_parser_entries,
                keep: None,
            },
            pg_description::Source {
                catalog: "pg_ts_template",
                entries: &ts_template_entries,
                keep: None,
            },
            pg_description::Source {
                catalog: "pg_ts_dict",
                entries: &ts_dict_entries,
                keep: None,
            },
            pg_description::Source {
                catalog: "pg_ts_config",
                entries: &ts_config_entries,
                keep: None,
            },
            pg_description::Source {
                catalog: "pg_am",
                entries: &am_entries,
                keep: None,
            },
            pg_description::Source {
                catalog: "pg_language",
                entries: &language_entries,
                keep: None,
            },
            pg_description::Source {
                catalog: "pg_namespace",
                entries: &namespace_entries,
                keep: None,
            },
        ]),
    )?;
    Ok(())
}

/// The `pg_am` OID of each index access method, by the name `pg_opclass.dat`
/// and `pg_opfamily.dat` write in their `opcmethod`/`opfmethod` columns.
///
/// `pg_am.dat` is vendored for its descriptions alone and defines no symbols —
/// `crabgresql-catalog`'s `catalogs::am` publishes those rows by hand — so the
/// OIDs are repeated here rather than resolved through the symbol table. The
/// two lists agreeing is checked
/// where it is observable: `crabgresql-catalog`'s tests join the served
/// `pg_opclass.opcmethod` against the served `pg_am.oid`, which is the claim a
/// client would find broken.
const INDEX_ACCESS_METHODS: [(&str, u32); 6] = [
    ("btree", 403),
    ("hash", 405),
    ("gist", 783),
    ("gin", 2742),
    ("brin", 3580),
    ("spgist", 4000),
];

/// The `pg_am` OID of the access method `name`. An unknown name is a new
/// method in a bumped `.dat`, which needs a `pg_am` row before its opclasses
/// can point at it — so it fails the build rather than emitting a dangling 0.
fn am_oid(name: &str) -> u32 {
    INDEX_ACCESS_METHODS
        .iter()
        .find(|(am, _)| *am == name)
        .map(|(_, oid)| *oid)
        .unwrap_or_else(|| panic!("no pg_am OID known for access method {name:?}"))
}

/// A `regproc` column as the `ProcRef` expression the generated file carries:
/// the name as written, plus the OID it resolves to. `-` is the catalog's
/// spelling of "no function" and resolves to 0, which prints back as `-`.
fn proc_ref(symbols: &SymbolTable, name: &str) -> String {
    let oid = symbols.resolve_name(Proc, name).unwrap_or(0);
    format!("ProcRef {{ oid: {oid}, name: {name:?} }}")
}

/// The `ProcRef` expression for a reference written in either spelling the
/// catalog data uses: a bare name where that is unambiguous, and a full
/// signature (`tsquery_phrase(tsquery,tsquery)`) where the name alone names
/// more than one function. `pg_amproc.amproc`, `pg_operator.oprcode` and
/// `pg_cast.castfunc` all take both.
///
/// The printed name drops the argument list, because that is what `regproc`
/// output renders — the signature is how the reference is *written*, not what
/// the column shows. `None` when nothing of that name is defined, which every
/// caller turns into its own build failure.
fn proc_ref_resolved(symbols: &SymbolTable, reference: &str) -> Option<String> {
    let oid = if reference.contains('(') {
        symbols.resolve_signature(Proc, reference)
    } else {
        symbols.resolve_name(Proc, reference)
    }?;
    let name = reference.split('(').next().unwrap_or(reference);
    Some(format!("ProcRef {{ oid: {oid}, name: {name:?} }}"))
}

/// Where upstream's generated OIDs begin. Below this sit the hand-assigned
/// ones; PostgreSQL reserves the band for exactly this purpose.
const FIRST_GENERATED_OID: u32 = 10000;

/// The OID counter a catalog's unnumbered entries are numbered from.
///
/// # Where the OIDs come from
///
/// Most `.dat` files spell out an `oid` for only the entries upstream's C code
/// names by symbol — `pg_opclass.dat` numbers 13 of 179, `pg_amop.dat` and
/// `pg_amproc.dat` number none at all. The rest are numbered by upstream's own
/// codegen, and the rule it follows is visible in the data:
///
/// > An entry's OID is its explicit `oid` if it has one; otherwise it is the
/// > next value of a counter that starts at [`FIRST_GENERATED_OID`] and
/// > advances **only** on entries without an explicit `oid`, in file order.
/// > The counter is per catalog, which is why `pg_cast` starts at the same
/// > number — an OID is unique within its catalog, not across catalogs.
///
/// That rule was checked against a PostgreSQL 18.4 install: reconstructing
/// `pg_opclass` from that release's `.dat` reproduced all 177 rows the release
/// ships, OID for OID. So the real OIDs are *derived from the vendored data*,
/// not transcribed from a running server and not read out of upstream's
/// source — the same clean-room path the rest of this crate takes.
///
/// The consequence for an emitter is the one that bites: [`Self::of`] must be
/// called for every entry it passes, whether or not a row comes out. Skipping
/// an entry *and* its number would shift every OID after it.
#[derive(Default)]
struct OidCounter {
    next: Option<u32>,
}

impl OidCounter {
    /// The OID of `e`, advancing the counter when the entry has none of its own.
    fn of(&mut self, e: &dat::Entry) -> u32 {
        match dat::oid_field(e, "oid") {
            0 => {
                let assigned = self.next.unwrap_or(FIRST_GENERATED_OID);
                self.next = Some(assigned + 1);
                assigned
            }
            explicit => explicit,
        }
    }
}
