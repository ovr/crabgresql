//! The five `pg_ts_*` catalogs: text-search parsers, dictionary templates,
//! dictionaries, configurations and the token maps that tie them together.
//!
//! # Two sources, because PostgreSQL has two
//!
//! The `.dat` half — the `default` parser, four templates, and the `simple`
//! dictionary and configuration — is generated (see `crabgresql-bki`'s `pg_ts`).
//! The other twenty-nine configurations and dictionaries a stock PostgreSQL
//! publishes are not in any `.dat`: `initdb` creates them by running the
//! generated `snowball_create.sql`, one `CREATE TEXT SEARCH DICTIONARY` and one
//! `CREATE TEXT SEARCH CONFIGURATION` per snowball language. [`SNOWBALL`]
//! reconstructs that half.
//!
//! # The deviation
//!
//! There is no stemmer here. `tsvector`/`tsquery` exist and `to_tsvector` works,
//! but it does not lex through these configurations — so `english` is described
//! and not used, and a stemmed search would not stem. That is a larger gap than
//! `pg_operator`'s: there the catalog describes something the resolver
//! reimplements, here it describes something nothing implements at all.
//!
//! Publishing the rows is still the better answer than publishing five empty
//! relations. `\dF` and `pg_dump` read them, `pg_ts_config_map` is the only
//! place the token-type numbering is visible, and a client asking whether
//! `english` exists gets upstream's answer either way — it is the *stemming*
//! that is missing, not the configuration.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::cols::*;
use crate::oids::*;
use crate::{
    PG_TS_CONFIG_MAP_ROWS, PG_TS_CONFIG_ROWS, PG_TS_DICT_ROWS, PG_TS_PARSER_ROWS,
    PG_TS_TEMPLATE_ROWS, SystemCatalog,
};

/// The snowball languages `initdb` builds a dictionary and a configuration for,
/// in the order `snowball_create.sql` creates them, with whether the language
/// ships a stop-word list.
///
/// Transcribed from the `snowball_create.sql` a PostgreSQL 18.4 installation
/// ships — data, in the same sense the vendored `.dat` files are, and the same
/// standing as `crabgresql-bki`'s hard-coded access-method OIDs. The order is
/// what assigns the OIDs; see [`snowball_dict_oid`].
const SNOWBALL: &[(&str, bool)] = &[
    ("arabic", false),
    ("armenian", false),
    ("basque", false),
    ("catalan", false),
    ("danish", true),
    ("dutch", true),
    ("english", true),
    ("estonian", false),
    ("finnish", true),
    ("french", true),
    ("german", true),
    ("greek", false),
    ("hindi", false),
    ("hungarian", true),
    ("indonesian", false),
    ("irish", false),
    ("italian", true),
    ("lithuanian", false),
    ("nepali", true),
    ("norwegian", true),
    ("portuguese", true),
    ("romanian", false),
    ("russian", true),
    ("serbian", false),
    ("spanish", true),
    ("swedish", true),
    ("tamil", false),
    ("turkish", true),
    ("yiddish", false),
];

/// `initdb` allocates these from the running OID counter as it executes
/// `snowball_create.sql`, so unlike everything generated from a `.dat` they
/// cannot be derived from vendored data — they were probed from PostgreSQL
/// 18.4, where they run 13638..13698 in exactly this shape:
///
/// ```text
/// 13638 dsnowball_init      13639 dsnowball_lexize   13640 template snowball
/// 13641 arabic_stem         13642 config arabic
/// 13643 armenian_stem       13644 config armenian     ... and so on, in pairs
/// ```
///
/// The band is deterministic for a major version and moves between them, the
/// same standing as the 12000-band relation OIDs in [`crate::registry`]. So
/// nothing pins these numbers in a test or a smoke file: what a client reads is
/// the name and the join.
fn snowball_dict_oid(i: usize) -> u32 {
    SNOWBALL_FIRST_DICT_OID + 2 * i as u32
}

fn snowball_config_oid(i: usize) -> u32 {
    snowball_dict_oid(i) + 1
}

/// The nineteen numbers are the `default` parser's, taken from the `simple`
/// configuration's own `pg_ts_config_map.dat` rows rather than invented; the
/// split is `snowball_create.sql`'s, which sends the six word-shaped tokens to
/// the language's stemmer and the other thirteen to `simple`. The four the
/// parser also emits and nobody maps — `blank`, `tag`, `protocol`, `entity` —
/// are absent here for the same reason they are absent upstream.
const STEMMED_TOKENS: [i32; 6] = [
    1,  // asciiword
    2,  // word
    10, // hword_part
    11, // hword_asciipart
    16, // asciihword
    17, // hword
];

fn simple_dict_oid() -> u32 {
    PG_TS_DICT_ROWS
        .iter()
        .find(|d| d.dictname == "simple")
        .map(|d| d.oid)
        .expect("pg_ts_dict.dat defines the simple dictionary")
}

fn default_parser_oid() -> u32 {
    PG_TS_PARSER_ROWS
        .iter()
        .find(|p| p.prsname == "default")
        .map(|p| p.oid)
        .expect("pg_ts_parser.dat defines the default parser")
}

pub(crate) fn pg_ts_parser_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_ts_parser",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("prsname", PgType::Name),
            col("prsnamespace", PgType::Oid),
            col("prsstart", REGPROC),
            col("prstoken", REGPROC),
            col("prsend", REGPROC),
            col("prsheadline", REGPROC),
            col("prslextype", REGPROC),
        ],
    )
}

pub(crate) fn pg_ts_parser_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    PG_TS_PARSER_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.prsname.to_string()),
                Value::Oid(PG_CATALOG_NAMESPACE_OID),
                regproc(r.prsstart),
                regproc(r.prstoken),
                regproc(r.prsend),
                regproc(r.prsheadline),
                regproc(r.prslextype),
            ]
        })
        .collect()
}

pub(crate) fn pg_ts_template_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_ts_template",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("tmplname", PgType::Name),
            col("tmplnamespace", PgType::Oid),
            col("tmplinit", REGPROC),
            col("tmpllexize", REGPROC),
        ],
    )
}

/// `snowball` is `initdb`'s rather than the `.dat`'s, and the twenty-nine
/// reconstructed dictionaries point at it — leaving it out would dangle every
/// one of them.
pub(crate) fn pg_ts_template_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let snowball = vec![
        Value::Oid(SNOWBALL_TEMPLATE_OID),
        Value::Text("snowball".to_string()),
        Value::Oid(PG_CATALOG_NAMESPACE_OID),
        regproc(SNOWBALL_INIT_PROC),
        regproc(SNOWBALL_LEXIZE_PROC),
    ];
    PG_TS_TEMPLATE_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.tmplname.to_string()),
                Value::Oid(PG_CATALOG_NAMESPACE_OID),
                regproc(r.tmplinit),
                regproc(r.tmpllexize),
            ]
        })
        .chain(std::iter::once(snowball))
        .collect()
}

pub(crate) fn pg_ts_dict_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_ts_dict",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("dictname", PgType::Name),
            col("dictnamespace", PgType::Oid),
            col("dictowner", PgType::Oid),
            col("dicttemplate", PgType::Oid),
            col("dictinitoption", PgType::Text),
        ],
    )
}

pub(crate) fn pg_ts_dict_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let dict = |oid: u32, name: String, template: u32, option: Value| {
        vec![
            Value::Oid(oid),
            Value::Text(name),
            Value::Oid(PG_CATALOG_NAMESPACE_OID),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Oid(template),
            option,
        ]
    };
    PG_TS_DICT_ROWS
        .iter()
        .map(|r| {
            dict(
                r.oid,
                r.dictname.to_string(),
                r.dicttemplate,
                text_or_null(r.dictinitoption),
            )
        })
        .chain(
            SNOWBALL
                .iter()
                .enumerate()
                .map(|(i, (language, stopwords))| {
                    // The option string `CREATE TEXT SEARCH DICTIONARY` stores, spelled
                    // the way PostgreSQL renders the clause back.
                    let option = if *stopwords {
                        format!("language = '{language}', stopwords = '{language}'")
                    } else {
                        format!("language = '{language}'")
                    };
                    dict(
                        snowball_dict_oid(i),
                        format!("{language}_stem"),
                        SNOWBALL_TEMPLATE_OID,
                        Value::Text(option),
                    )
                }),
        )
        .collect()
}

pub(crate) fn pg_ts_config_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_ts_config",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("cfgname", PgType::Name),
            col("cfgnamespace", PgType::Oid),
            col("cfgowner", PgType::Oid),
            col("cfgparser", PgType::Oid),
        ],
    )
}

/// All thirty use the `default` parser: snowball supplies dictionaries, not a
/// parser.
pub(crate) fn pg_ts_config_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let parser = default_parser_oid();
    let config = |oid: u32, name: String, parser: u32| {
        vec![
            Value::Oid(oid),
            Value::Text(name),
            Value::Oid(PG_CATALOG_NAMESPACE_OID),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Oid(parser),
        ]
    };
    PG_TS_CONFIG_ROWS
        .iter()
        .map(|r| config(r.oid, r.cfgname.to_string(), r.cfgparser))
        .chain(SNOWBALL.iter().enumerate().map(|(i, (language, _))| {
            config(snowball_config_oid(i), (*language).to_string(), parser)
        }))
        .collect()
}

pub(crate) fn pg_ts_config_map_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_ts_config_map",
        "pg_catalog",
        vec![
            col("mapcfg", PgType::Oid),
            col("maptokentype", PgType::Int4),
            col("mapseqno", PgType::Int4),
            col("mapdict", PgType::Oid),
        ],
    )
}

/// Nineteen rows per configuration — six word-shaped token types to the
/// language's stemmer and thirteen to `simple`, which is what
/// `snowball_create.sql`'s three `ADD MAPPING` statements add up to.
///
/// Every row has `mapseqno = 1`: no built-in configuration lists a second
/// dictionary for a token.
pub(crate) fn pg_ts_config_map_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let simple = simple_dict_oid();
    // The token types to map, taken from the `simple` configuration rather than
    // written out again.
    let mut tokens: Vec<i32> = PG_TS_CONFIG_MAP_ROWS
        .iter()
        .map(|r| r.maptokentype)
        .collect();
    tokens.sort_unstable();
    tokens.dedup();

    let row = |cfg: u32, token: i32, dict: u32| {
        vec![
            Value::Oid(cfg),
            Value::Int4(token),
            Value::Int4(1),
            Value::Oid(dict),
        ]
    };
    PG_TS_CONFIG_MAP_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.mapcfg),
                Value::Int4(r.maptokentype),
                Value::Int4(r.mapseqno),
                Value::Oid(r.mapdict),
            ]
        })
        .chain(SNOWBALL.iter().enumerate().flat_map(|(i, _)| {
            let (cfg, stem) = (snowball_config_oid(i), snowball_dict_oid(i));
            tokens.clone().into_iter().map(move |token| {
                let dict = if STEMMED_TOKENS.contains(&token) {
                    stem
                } else {
                    simple
                };
                row(cfg, token, dict)
            })
        }))
        .collect()
}

/// The comments `snowball_create.sql` attaches to the objects it creates:
/// one on the template, and one on each language's dictionary and
/// configuration. `initdb` writes them with `COMMENT ON`, so unlike every other
/// bootstrap description they come from no `.dat` and are built here.
///
/// Returned as `(catalog name, objoid, description)` — the relation OIDs belong
/// to [`crate::registry`], not to this module. The formatted strings are leaked
/// once, behind the `OnceLock` in [`super::description`], because a description
/// lives as long as the process either way.
pub(crate) fn snowball_descriptions() -> Vec<(&'static str, u32, &'static str)> {
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let mut rows = vec![("pg_ts_template", SNOWBALL_TEMPLATE_OID, "snowball stemmer")];
    for (i, (language, _)) in SNOWBALL.iter().enumerate() {
        rows.push((
            "pg_ts_dict",
            snowball_dict_oid(i),
            leak(format!("snowball stemmer for {language} language")),
        ));
        rows.push((
            "pg_ts_config",
            snowball_config_oid(i),
            leak(format!("configuration for {language} language")),
        ));
    }
    rows
}
