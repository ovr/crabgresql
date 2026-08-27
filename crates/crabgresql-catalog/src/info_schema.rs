//! The five domains `initdb` creates in `information_schema`, and the array
//! type each one carries.
//!
//! The SQL standard types every `information_schema` column as a domain rather
//! than as a base type, and PostgreSQL follows it: `table_name` is
//! `information_schema.sql_identifier`, not `name`. So the definition text in
//! [`crate::views::definitions`] casts some ninety columns into these five, and
//! none of those casts can bind until the domains exist as catalog rows.
//!
//! The OIDs are PostgreSQL 18.4's. Like [`crate::oids::PLPGSQL_EXTENSION_OID`]
//! and the snowball band, they come from `initdb`'s running counter rather than
//! from a `.dat` file — deterministic for a major version rather than fixed
//! forever, and reusing them still beats inventing numbers, because a client
//! that hard-codes one hard-codes these.
//!
//! Only the columns that differ per domain are fields. Everything else a domain
//! row holds is copied from the base type by
//! [`crate::catalogs::types::pg_type_domain_row`], which serves `CREATE DOMAIN`
//! too — the two cannot drift because there is one builder.

use crabgresql_storage_api::{DomainCheck, DomainInfo};
use crabgresql_types::PgType;

use crate::oids::INFORMATION_SCHEMA_NAMESPACE_OID;
use crate::{CatalogConstraint, CatalogDomain, CatalogDomainCheck, CatalogUserType};

/// One `initdb`-created domain of `information_schema`.
pub(crate) struct BootstrapDomain {
    pub(crate) oid: u32,
    pub(crate) name: &'static str,
    /// The domain's array type — a `typtype = 'b'` row of its own, which
    /// `CREATE DOMAIN` does not get here (see [`CatalogDomain`]).
    pub(crate) array_oid: u32,
    pub(crate) array_name: &'static str,
    pub(crate) base: PgType,
    /// The modifier **as declared** (`varchar(3)` is `3`), not in `atttypmod`
    /// encoding — the same convention [`CatalogDomain::typmod`] carries, since
    /// the shared row builder is what applies `pg_typmod`.
    pub(crate) typmod: i32,
    /// `typcollation`. Spelled out rather than inherited from the base: the
    /// three text-family domains are `C` (950), while `varchar` itself is the
    /// default collation (100), so inheriting would report the wrong one.
    pub(crate) collation: u32,
    /// `typdefault`, as the canonical SQL PostgreSQL stores.
    pub(crate) default: Option<&'static str>,
    /// The domain's `CHECK`, for the two that have one.
    pub(crate) check: Option<BootstrapCheck>,
}

/// The `CHECK` of a [`BootstrapDomain`], as `pg_constraint` publishes it.
pub(crate) struct BootstrapCheck {
    pub(crate) oid: u32,
    pub(crate) name: &'static str,
    /// The predicate over `VALUE` as stored SQL — `pg_constraint.conbin`, in
    /// the non-pretty spelling `pg_get_constraintdef` wraps in `CHECK (…)`.
    pub(crate) expr: &'static str,
}

/// `pg_collation.oid` of `C`, which is what every text-family
/// `information_schema` domain sorts under.
const C_COLLATION: u32 = crabgresql_types::collation::C_COLLATION_OID;

/// The four domains the views this build serves are typed in, plus
/// [`TIME_STAMP`], which only `routines` and `triggers` use. Named so a view's
/// column list reads as PostgreSQL's `\d` prints it rather than as digits.
pub(crate) const CARDINAL_NUMBER: u32 = 13713;
pub(crate) const CHARACTER_DATA: u32 = 13716;
pub(crate) const SQL_IDENTIFIER: u32 = 13718;
pub(crate) const TIME_STAMP: u32 = 13724;
pub(crate) const YES_OR_NO: u32 = 13726;

/// The five domains, ordered by OID as `initdb` created them.
pub(crate) static DOMAINS: &[BootstrapDomain] = &[
    BootstrapDomain {
        oid: CARDINAL_NUMBER,
        name: "cardinal_number",
        array_oid: 13712,
        array_name: "_cardinal_number",
        base: PgType::Int4,
        typmod: -1,
        collation: 0,
        default: None,
        check: Some(BootstrapCheck {
            oid: 13714,
            name: "cardinal_number_domain_check",
            expr: "(VALUE >= 0)",
        }),
    },
    BootstrapDomain {
        oid: CHARACTER_DATA,
        name: "character_data",
        array_oid: 13715,
        array_name: "_character_data",
        base: PgType::Varchar,
        typmod: -1,
        collation: C_COLLATION,
        default: None,
        check: None,
    },
    BootstrapDomain {
        oid: SQL_IDENTIFIER,
        name: "sql_identifier",
        array_oid: 13717,
        array_name: "_sql_identifier",
        base: PgType::Name,
        typmod: -1,
        collation: C_COLLATION,
        default: None,
        check: None,
    },
    BootstrapDomain {
        oid: TIME_STAMP,
        name: "time_stamp",
        array_oid: 13723,
        array_name: "_time_stamp",
        base: PgType::TimestampTz,
        typmod: 2,
        collation: 0,
        default: Some("CURRENT_TIMESTAMP(2)"),
        check: None,
    },
    BootstrapDomain {
        oid: YES_OR_NO,
        name: "yes_or_no",
        array_oid: 13725,
        array_name: "_yes_or_no",
        base: PgType::Varchar,
        typmod: 3,
        collation: C_COLLATION,
        default: None,
        check: Some(BootstrapCheck {
            oid: 13727,
            name: "yes_or_no_check",
            expr: "((VALUE)::text = ANY ((ARRAY['YES'::character varying, \
                   'NO'::character varying])::text[]))",
        }),
    },
];

/// The namespace all five live in.
pub(crate) const NAMESPACE_OID: u32 = INFORMATION_SCHEMA_NAMESPACE_OID;

pub(crate) const NAMESPACE: &str = "information_schema";

/// The domain `name` identifies, or `None` for a name none of them has.
/// Case-sensitive: all five are lower-case, and so is every reference to them.
pub(crate) fn by_name(name: &str) -> Option<&'static BootstrapDomain> {
    DOMAINS.iter().find(|d| d.name == name)
}

/// The domain `oid` identifies, or `None` for an OID none of them has.
pub(crate) fn by_oid(oid: u32) -> Option<&'static BootstrapDomain> {
    DOMAINS.iter().find(|d| d.oid == oid)
}

/// The binder's view of one of these domains, by unqualified name.
///
/// Unqualified because that is how a cast target reaches the type catalog: the
/// binder resolves a type name by its **last part**, as it does a function's.
/// A `CREATE DOMAIN sql_identifier` in `public` therefore has to be consulted
/// first — see the [`crate::SystemCatalog::user_type_oid`] note.
pub fn domain_info_by_name(name: &str) -> Option<DomainInfo> {
    by_name(name).map(BootstrapDomain::domain_info)
}

/// The binder's view of one of these domains, by OID.
pub fn domain_info_by_oid(oid: u32) -> Option<DomainInfo> {
    by_oid(oid).map(BootstrapDomain::domain_info)
}

/// The two `CHECK`s these domains carry, as `pg_constraint` rows.
///
/// Their OIDs are `initdb`'s, so they sit far below the band
/// [`crate::SystemCatalog::constraint_oids`] hands out and cannot take part in
/// its positional numbering. Every reader therefore has to consult this list
/// *before* that one — see [`crate::SystemCatalog::constraint_def`].
pub(crate) fn constraints() -> Vec<CatalogConstraint> {
    DOMAINS
        .iter()
        .filter_map(|d| Some((d, d.check.as_ref()?)))
        .map(|(d, c)| CatalogConstraint {
            oid: c.oid,
            name: c.name.to_string(),
            contype: "c",
            namespace: NAMESPACE.to_string(),
            // A domain constraint constrains a type, not a relation.
            table_oid: 0,
            type_oid: d.oid,
            index_oid: 0,
            columns: Vec::new(),
            expr: Some(c.expr.to_string()),
            validated: true,
            islocal: true,
            inhcount: 0,
        })
        .collect()
}

impl BootstrapDomain {
    /// This domain as the binder reads it. `typmod` is the declared modifier,
    /// the convention [`DomainInfo::typmod`] carries and the one
    /// `base_type_and_typmod` hands to the length-applying coercions.
    ///
    /// The qualified name: PostgreSQL spells it that way in the 23514 a failed
    /// `CHECK` raises, because the type is not visible on the search path.
    pub(crate) fn domain_info(&self) -> DomainInfo {
        DomainInfo {
            oid: self.oid,
            name: format!("{NAMESPACE}.{}", self.name),
            base: self.base,
            typmod: self.typmod,
            collation: match self.collation {
                0 => None,
                explicit => Some(explicit),
            },
            not_null: false,
            default: self.default.map(str::to_string),
            checks: self
                .check
                .iter()
                .map(|c| DomainCheck {
                    name: c.name.to_string(),
                    expr: c.expr.to_string(),
                    validated: true,
                })
                .collect(),
        }
    }

    /// This domain in the shape the shared `pg_type` row builder takes, so a
    /// bootstrap domain and a `CREATE DOMAIN` go through exactly one code path.
    ///
    /// `basetype` and `resolved_basetype` are the same OID because no bootstrap
    /// domain is over another domain — PostgreSQL's own chain is one link long
    /// here.
    pub(crate) fn as_catalog_type(&self) -> CatalogUserType {
        let base = self.base.oid();
        CatalogUserType {
            oid: self.oid,
            name: self.name.to_string(),
            enum_labels: None,
            domain: Some(CatalogDomain {
                basetype: base,
                resolved_basetype: base,
                typmod: self.typmod,
                collation: self.collation,
                // None of the five is `NOT NULL`; the standard leaves every
                // `information_schema` column nullable.
                not_null: false,
                default: self.default.map(str::to_string),
                checks: self
                    .check
                    .iter()
                    .map(|c| CatalogDomainCheck {
                        name: c.name.to_string(),
                        expr: c.expr.to_string(),
                        validated: true,
                    })
                    .collect(),
            }),
        }
    }
}
