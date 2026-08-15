//! The five `pg_ts_*` catalogs' bootstrap rows: text-search parsers,
//! dictionary templates, dictionaries, configurations and their token maps.
//!
//! # What the data does and does not contain
//!
//! These five `.dat` files describe exactly one text-search setup: the
//! `default` parser, four dictionary templates, and the `simple` dictionary and
//! configuration built from them. A stock PostgreSQL publishes thirty
//! configurations and thirty dictionaries, and the other twenty-nine of each
//! are not in any `.dat` at all — `initdb` creates them by running the
//! generated `snowball_create.sql`, which is SQL rather than catalog data.
//!
//! So this module emits the `.dat` half, and `crabgresql-catalog`'s
//! `catalogs::textsearch` adds the snowball half from the language list. The
//! split follows what upstream itself does, and each half says where its rows
//! come from.
//!
//! References inside these five files (`cfgparser`, `dicttemplate`, `mapcfg`,
//! `mapdict`) point at each other by name and never leave the group, so they
//! resolve through local maps rather than through the symbol table — which is
//! reserved for the cross-catalog cycles that need two phases. The `regproc`
//! columns do go through it.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::dat::{Entry, get, oid_field, str_field};
use crate::symbols::SymbolTable;

/// Everything the five files emit, in one string per row array.
pub struct Emitted {
    pub parsers: String,
    pub templates: String,
    pub dicts: String,
    pub configs: String,
    pub config_map: String,
}

/// Emit all five row arrays. They are done together because the last three
/// reference the first two by name.
pub fn emit(
    parsers: &[Entry],
    templates: &[Entry],
    dicts: &[Entry],
    configs: &[Entry],
    config_map: &[Entry],
    symbols: &SymbolTable,
) -> Emitted {
    let by_name = |entries: &[Entry], key: &str| -> HashMap<String, u32> {
        entries
            .iter()
            .filter_map(|e| Some((get(e, key)?.to_string(), oid_field(e, "oid"))))
            .collect()
    };
    let parser_oids = by_name(parsers, "prsname");
    let template_oids = by_name(templates, "tmplname");
    let dict_oids = by_name(dicts, "dictname");
    let config_oids = by_name(configs, "cfgname");
    let lookup = |map: &HashMap<String, u32>, what: &str, name: &str| {
        *map.get(name)
            .unwrap_or_else(|| panic!("pg_ts_*: {what} {name:?} names no entry"))
    };
    let regproc = |e: &Entry, key: &str| crate::proc_ref(symbols, str_field(e, key, "-"));

    let mut parsers_out = header("pg_ts_parser", "PG_TS_PARSER_ROWS", "PgTsParserRow");
    for e in parsers {
        let _ = writeln!(
            parsers_out,
            "    PgTsParserRow {{ oid: {oid}, prsname: {name:?}, prsstart: {start}, \
prstoken: {token}, prsend: {end}, prsheadline: {headline}, prslextype: {lextype} }},",
            oid = oid_field(e, "oid"),
            name = str_field(e, "prsname", ""),
            start = regproc(e, "prsstart"),
            token = regproc(e, "prstoken"),
            end = regproc(e, "prsend"),
            headline = regproc(e, "prsheadline"),
            lextype = regproc(e, "prslextype"),
        );
    }

    let mut templates_out = header("pg_ts_template", "PG_TS_TEMPLATE_ROWS", "PgTsTemplateRow");
    for e in templates {
        let _ = writeln!(
            templates_out,
            "    PgTsTemplateRow {{ oid: {oid}, tmplname: {name:?}, tmplinit: {init}, \
tmpllexize: {lexize} }},",
            oid = oid_field(e, "oid"),
            name = str_field(e, "tmplname", ""),
            init = regproc(e, "tmplinit"),
            lexize = regproc(e, "tmpllexize"),
        );
    }

    let mut dicts_out = header("pg_ts_dict", "PG_TS_DICT_ROWS", "PgTsDictRow");
    for e in dicts {
        let _ = writeln!(
            dicts_out,
            "    PgTsDictRow {{ oid: {oid}, dictname: {name:?}, dicttemplate: {template}, \
dictinitoption: {option:?} }},",
            oid = oid_field(e, "oid"),
            name = str_field(e, "dictname", ""),
            template = lookup(
                &template_oids,
                "dicttemplate",
                str_field(e, "dicttemplate", "")
            ),
            // The empty string is this row array's spelling of NULL, which is
            // what the `simple` dictionary stores: it takes no options.
            option = str_field(e, "dictinitoption", ""),
        );
    }

    let mut configs_out = header("pg_ts_config", "PG_TS_CONFIG_ROWS", "PgTsConfigRow");
    for e in configs {
        let _ = writeln!(
            configs_out,
            "    PgTsConfigRow {{ oid: {oid}, cfgname: {name:?}, cfgparser: {parser} }},",
            oid = oid_field(e, "oid"),
            name = str_field(e, "cfgname", ""),
            parser = lookup(&parser_oids, "cfgparser", str_field(e, "cfgparser", "")),
        );
    }

    // `pg_ts_config_map` has no `oid` column at all: a row is keyed by
    // (mapcfg, maptokentype, mapseqno), which is what makes a token's
    // dictionary list ordered.
    let mut map_out = header(
        "pg_ts_config_map",
        "PG_TS_CONFIG_MAP_ROWS",
        "PgTsConfigMapRow",
    );
    for e in config_map {
        let _ = writeln!(
            map_out,
            "    PgTsConfigMapRow {{ mapcfg: {cfg}, maptokentype: {token}, mapseqno: {seqno}, \
mapdict: {dict} }},",
            cfg = lookup(&config_oids, "mapcfg", str_field(e, "mapcfg", "")),
            token = int_field(e, "maptokentype"),
            seqno = int_field(e, "mapseqno"),
            dict = lookup(&dict_oids, "mapdict", str_field(e, "mapdict", "")),
        );
    }

    Emitted {
        parsers: close(parsers_out),
        templates: close(templates_out),
        dicts: close(dicts_out),
        configs: close(configs_out),
        config_map: close(map_out),
    }
}

fn header(dat: &str, array: &str, row: &str) -> String {
    format!(
        "// @generated by crabgresql-bki from vendor/postgres/catalog/{dat}.dat\n\
         pub static {array}: &[{row}] = &[\n"
    )
}

fn close(mut out: String) -> String {
    out.push_str("];\n\n");
    out
}

fn int_field(e: &Entry, key: &str) -> i32 {
    str_field(e, key, "")
        .parse()
        .unwrap_or_else(|_| panic!("pg_ts_config_map: bad {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dat::parse_dat;
    use crate::pg_proc;
    use crate::symbols::SymbolKind::Type;

    /// The five files' worth of entries, plus the functions they name.
    struct Fixture {
        parsers: Vec<Entry>,
        templates: Vec<Entry>,
        dicts: Vec<Entry>,
        configs: Vec<Entry>,
        config_map: Vec<Entry>,
        symbols: SymbolTable,
    }

    impl Fixture {
        fn emit(&self) -> Emitted {
            emit(
                &self.parsers,
                &self.templates,
                &self.dicts,
                &self.configs,
                &self.config_map,
                &self.symbols,
            )
        }
    }

    fn fixture() -> Fixture {
        let parsers = parse_dat(
            "[{ oid => '3722', prsname => 'default', prsstart => 'prsd_start', \
             prstoken => 'prsd_nexttoken', prsend => 'prsd_end', \
             prsheadline => 'prsd_headline', prslextype => 'prsd_lextype' }]",
        );
        let templates = parse_dat(
            "[{ oid => '3727', tmplname => 'simple', tmplinit => 'dsimple_init', \
             tmpllexize => 'dsimple_lexize' }]",
        );
        let dicts = parse_dat(
            "[{ oid => '3765', dictname => 'simple', dicttemplate => 'simple', \
             dictinitoption => _null_ }]",
        );
        let configs = parse_dat("[{ oid => '3748', cfgname => 'simple', cfgparser => 'default' }]");
        let config_map = parse_dat(
            "[{ mapcfg => 'simple', maptokentype => '1', mapseqno => '1', mapdict => 'simple' }]",
        );
        let mut symbols = SymbolTable::default();
        symbols.define_name(Type, "internal", 2281);
        pg_proc::define_symbols(
            &parse_dat(
                "[{ oid => '3723', proname => 'prsd_start' },\n\
                  { oid => '3724', proname => 'prsd_nexttoken' },\n\
                  { oid => '3719', proname => 'prsd_end' },\n\
                  { oid => '3721', proname => 'prsd_headline' },\n\
                  { oid => '3720', proname => 'prsd_lextype' },\n\
                  { oid => '3725', proname => 'dsimple_init' },\n\
                  { oid => '3726', proname => 'dsimple_lexize' }]",
            ),
            &mut symbols,
        );
        Fixture {
            parsers,
            templates,
            dicts,
            configs,
            config_map,
            symbols,
        }
    }

    #[test]
    fn the_five_relations_reference_each_other_by_name() {
        let out = fixture().emit();
        assert!(
            out.parsers
                .contains("prsstart: ProcRef { oid: 3723, name: \"prsd_start\" }")
        );
        assert!(
            out.templates
                .contains("tmplinit: ProcRef { oid: 3725, name: \"dsimple_init\" }")
        );
        // The dictionary names its template and the configuration its parser,
        // each resolved to the OID the other file assigns.
        assert!(out.dicts.contains("dicttemplate: 3727"));
        assert!(out.configs.contains("cfgparser: 3722"));
        // A map row names both the configuration and the dictionary, and has
        // no OID of its own.
        assert!(out.config_map.contains(
            "PgTsConfigMapRow { mapcfg: 3748, maptokentype: 1, mapseqno: 1, mapdict: 3765 }"
        ));
        // `simple` takes no options, which the catalog stores as NULL.
        assert!(out.dicts.contains("dictinitoption: \"\""));
    }

    #[test]
    #[should_panic(expected = "names no entry")]
    fn an_unresolvable_template_fails_the_build() {
        let mut fixture = fixture();
        fixture.dicts =
            parse_dat("[{ oid => '3765', dictname => 'simple', dicttemplate => 'nosuch' }]");
        fixture.emit();
    }
}
