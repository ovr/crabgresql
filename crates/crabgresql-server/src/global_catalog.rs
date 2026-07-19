//! Server-lifetime catalog for user-defined types, functions, and casts.
//!
//! Unlike the per-session temp-table overlay ([`crate::catalog`]), these object
//! classes are persistent for PG (`pg_type`/`pg_proc`/`pg_cast`), so the catalog
//! is shared across every connection behind an `RwLock` and lives as long as the
//! server. It is intentionally minimal: it stores just enough to reproduce the
//! observable DDL behaviour of the type-bootstrap sequence (shell types, `LANGUAGE
//! internal` I/O functions, `CREATE CAST`, and `DROP TYPE ... CASCADE` with the
//! right NOTICE/DETAIL), not full query-time evaluation over user types.

use std::collections::HashMap;
use std::sync::RwLock;

use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{EnumInfo, TypeCatalog, UserCast, UserType};
use crabgresql_types::PgType;

use crate::error::PgError;

/// User-created objects get OIDs at or above 16384; lower values are reserved
/// for built-ins, matching the OID ranges observed from a running server.
const FIRST_USER_OID: u32 = 16384;

/// A type as referenced from a function signature or a cast: a built-in type, a
/// user-defined type (by lowercased name), or the `cstring` I/O pseudo-type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeRef {
    Builtin(PgType),
    User(String),
    Cstring,
}

impl TypeRef {
    /// The name as PG spells it in dependency / cascade messages: `bigint` for
    /// int8, and the type's own name for a user type (observed behaviour).
    fn display_name(&self) -> String {
        match self {
            TypeRef::Builtin(t) => t.name().to_string(),
            TypeRef::User(name) => name.clone(),
            TypeRef::Cstring => "cstring".to_string(),
        }
    }

    /// Physical storage width used for `WITHOUT FUNCTION` compatibility.
    fn typlen(&self, cat: &CatalogInner) -> Option<i32> {
        match self {
            TypeRef::Builtin(t) => Some(t.typlen() as i32),
            TypeRef::User(name) => cat.types.get(name).map(|e| e.typlen),
            TypeRef::Cstring => None,
        }
    }
}

/// A NOTICE the catalog asks the server to emit after a DDL command.
#[derive(Debug)]
pub struct CatalogNotice {
    pub message: String,
    pub detail: Option<String>,
    /// 1-based (line, column) of the token this NOTICE points at, when PG
    /// renders a `LINE n:` cursor excerpt (e.g. the argument type of an
    /// "argument type … is only a shell" NOTICE). Converted to a wire character
    /// offset when the NOTICE is sent.
    pub position: Option<(u64, u64)>,
}

impl CatalogNotice {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
            position: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn with_position(mut self, position: (u64, u64)) -> Self {
        self.position = Some(position);
        self
    }
}

/// Identifies a non-type dependent of a type, for `DROP TYPE ... CASCADE`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DepId {
    Func(u32),
    Cast(u32),
}

struct TypeEntry {
    oid: u32,
    /// `false` while the type is a bare shell (`CREATE TYPE name;`).
    defined: bool,
    /// Physical width (`pg_type.typlen`); -1 for variable-length.
    typlen: i32,
    /// Backing builtin representation (from a `LIKE` clause), so a
    /// `WITHOUT FUNCTION` cast can reinterpret values as this type. `None` when
    /// the type declared only an `INTERNALLENGTH`.
    backing: Option<PgType>,
    /// The labels of a `CREATE TYPE ... AS ENUM`, in definition (= sort) order.
    /// `None` for shell and base types; `Some(..)` marks the type as an enum.
    enum_labels: Option<Vec<String>>,
    /// Objects that depend on this type, in creation order — the cascade set.
    dependents: Vec<DepId>,
}

struct FuncEntry {
    oid: u32,
    name: String,
    args: Vec<TypeRef>,
}

impl FuncEntry {
    /// PG's `function name(argtype, ...)` object description.
    fn describe(&self) -> String {
        let args: Vec<String> = self.args.iter().map(TypeRef::display_name).collect();
        format!("function {}({})", self.name, args.join(", "))
    }
}

struct CastEntry {
    oid: u32,
    source: TypeRef,
    target: TypeRef,
    /// `WITHOUT FUNCTION` — a binary-coercible (bit-reinterpret) cast.
    without_function: bool,
}

impl CastEntry {
    /// PG's `cast from SOURCE to TARGET` object description.
    fn describe(&self) -> String {
        format!(
            "cast from {} to {}",
            self.source.display_name(),
            self.target.display_name()
        )
    }
}

#[derive(Default)]
struct CatalogInner {
    next_oid: u32,
    /// Keyed by lowercased type name.
    types: HashMap<String, TypeEntry>,
    funcs: Vec<FuncEntry>,
    casts: Vec<CastEntry>,
}

impl CatalogInner {
    fn alloc_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }

    /// Record that `dep` depends on the (possibly shell) type named `name`,
    /// avoiding duplicate edges when a signature mentions the type twice.
    fn add_dependent(&mut self, name: &str, dep: DepId) {
        if let Some(entry) = self.types.get_mut(name)
            && !entry.dependents.contains(&dep)
        {
            entry.dependents.push(dep);
        }
    }

    fn func(&self, oid: u32) -> Option<&FuncEntry> {
        self.funcs.iter().find(|f| f.oid == oid)
    }

    fn cast(&self, oid: u32) -> Option<&CastEntry> {
        self.casts.iter().find(|c| c.oid == oid)
    }

    /// The name of a user type by OID (reverse of the `types` map).
    fn type_name_by_oid(&self, oid: u32) -> Option<&str> {
        self.types
            .iter()
            .find(|(_, e)| e.oid == oid)
            .map(|(name, _)| name.as_str())
    }

    /// A query-time `PgType` as the catalog's [`TypeRef`], so it can be matched
    /// against a stored cast's source/target. `None` when a user OID is unknown.
    fn type_ref_of(&self, ty: PgType) -> Option<TypeRef> {
        match ty {
            PgType::User(oid) => self
                .type_name_by_oid(oid)
                .map(|name| TypeRef::User(name.to_string())),
            other => Some(TypeRef::Builtin(other)),
        }
    }
}

/// The shared user-object catalog. Cloned (as an `Arc`) into every connection.
pub struct GlobalCatalog {
    inner: RwLock<CatalogInner>,
}

impl Default for GlobalCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalCatalog {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(CatalogInner {
                next_oid: FIRST_USER_OID,
                ..Default::default()
            }),
        }
    }

    /// Whether `name` (lowercased) resolves to a user-defined type — used to
    /// resolve a SQL type name to a [`TypeRef`].
    pub fn is_user_type(&self, name: &str) -> bool {
        self.inner.read().unwrap().types.contains_key(name)
    }

    /// Physical width of a user type, for a `LIKE` clause referencing it.
    pub fn user_type_typlen(&self, name: &str) -> Option<i32> {
        self.inner.read().unwrap().types.get(name).map(|e| e.typlen)
    }

    /// Backing builtin of a user type, propagated when another type is `LIKE` it.
    pub fn user_type_backing(&self, name: &str) -> Option<PgType> {
        self.inner.read().unwrap().types.get(name).and_then(|e| e.backing)
    }

    /// `CREATE TYPE name;` — register a shell type. Re-declaring any existing
    /// type (shell or fully defined) is a duplicate-object error, as in PG.
    pub fn create_shell_type(&self, name: &str) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self.inner.write().unwrap();
        if cat.types.contains_key(name) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("type \"{name}\" already exists"),
            ));
        }
        let oid = cat.alloc_oid();
        cat.types.insert(
            name.to_string(),
            TypeEntry {
                oid,
                defined: false,
                typlen: -1,
                backing: None,
                enum_labels: None,
                dependents: Vec::new(),
            },
        );
        Ok(Vec::new())
    }

    /// `CREATE TYPE name (...)` — fill in a previously declared shell (or create
    /// a defined type directly). `typlen`/`backing` are derived from the
    /// LIKE/INTERNALLENGTH options by the caller; `backing` is the `LIKE` builtin
    /// used for query-time `WITHOUT FUNCTION` casts.
    pub fn define_type(
        &self,
        name: &str,
        typlen: i32,
        backing: Option<PgType>,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self.inner.write().unwrap();
        match cat.types.get_mut(name) {
            Some(entry) if entry.defined => Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("type \"{name}\" already exists"),
            )),
            Some(entry) => {
                entry.defined = true;
                entry.typlen = typlen;
                entry.backing = backing;
                Ok(Vec::new())
            }
            None => {
                let oid = cat.alloc_oid();
                cat.types.insert(
                    name.to_string(),
                    TypeEntry {
                        oid,
                        defined: true,
                        typlen,
                        backing,
                        enum_labels: None,
                        dependents: Vec::new(),
                    },
                );
                Ok(Vec::new())
            }
        }
    }

    /// `CREATE TYPE name AS ENUM ('a', 'b', ...)`. Labels are stored verbatim
    /// (case-sensitive) in definition order, which is also their sort order.
    /// Re-declaring an existing type is a duplicate-object error; a label that
    /// repeats within the list is rejected as PG's `enum_in` bootstrap does.
    pub fn create_enum_type(
        &self,
        name: &str,
        labels: Vec<String>,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self.inner.write().unwrap();
        if cat.types.contains_key(name) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("type \"{name}\" already exists"),
            ));
        }
        // A label may not appear twice; PG reports the first repeat by name.
        let mut seen = std::collections::HashSet::with_capacity(labels.len());
        for label in &labels {
            if !seen.insert(label.as_str()) {
                return Err(PgError::new(
                    sqlstate::DUPLICATE_OBJECT,
                    format!("enum label \"{label}\" used more than once"),
                ));
            }
        }
        let oid = cat.alloc_oid();
        cat.types.insert(
            name.to_string(),
            TypeEntry {
                oid,
                defined: true,
                // PG enums are a fixed 4-byte OID-backed type.
                typlen: 4,
                backing: None,
                enum_labels: Some(labels),
                dependents: Vec::new(),
            },
        );
        Ok(Vec::new())
    }

    /// `CREATE FUNCTION ... LANGUAGE internal AS '<internal_name>'`. Validates the
    /// internal name, registers the function, records its type dependencies, and
    /// returns any "argument/return type is only a shell" NOTICEs.
    /// `args` pairs each argument type with the 1-based (line, column) of its
    /// type token, when known — used to point the argument-shell NOTICE's caret.
    pub fn create_function(
        &self,
        name: &str,
        args: Vec<(TypeRef, Option<(u64, u64)>)>,
        ret: TypeRef,
        internal_name: &str,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        if !is_known_internal(internal_name) {
            return Err(PgError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!("there is no built-in function named \"{internal_name}\""),
            ));
        }

        let arg_types: Vec<TypeRef> = args.iter().map(|(ty, _)| ty.clone()).collect();

        let mut cat = self.inner.write().unwrap();

        // A function may not be redefined with the same name and argument types.
        if cat.funcs.iter().any(|f| f.name == name && f.args == arg_types) {
            let arglist: Vec<String> = arg_types.iter().map(TypeRef::display_name).collect();
            return Err(PgError::new(
                sqlstate::DUPLICATE_FUNCTION,
                format!(
                    "function {name}({}) already exists with same argument types",
                    arglist.join(", ")
                ),
            ));
        }

        let mut notices = Vec::new();

        // PG reports a shell reference for the return type first, then arguments.
        // The return-type NOTICE carries no cursor position (PG prints no caret
        // for it); each argument NOTICE points at its type token.
        if let TypeRef::User(tname) = &ret
            && cat.types.get(tname).is_some_and(|e| !e.defined)
        {
            notices.push(CatalogNotice::new(format!(
                "return type {tname} is only a shell"
            )));
        }
        for (arg, position) in &args {
            if let TypeRef::User(tname) = arg
                && cat.types.get(tname).is_some_and(|e| !e.defined)
            {
                let mut notice =
                    CatalogNotice::new(format!("argument type {tname} is only a shell"));
                if let Some(pos) = position {
                    notice = notice.with_position(*pos);
                }
                notices.push(notice);
            }
        }

        let oid = cat.alloc_oid();
        // Every user type mentioned in the signature depends on this function.
        for tref in std::iter::once(&ret).chain(arg_types.iter()) {
            if let TypeRef::User(tname) = tref {
                cat.add_dependent(tname, DepId::Func(oid));
            }
        }
        cat.funcs.push(FuncEntry {
            oid,
            name: name.to_string(),
            args: arg_types,
        });
        Ok(notices)
    }

    /// `CREATE CAST (source AS target) ...`. Only binary-coercible
    /// (`WITHOUT FUNCTION`) casts are supported for now.
    pub fn create_cast(
        &self,
        source: TypeRef,
        target: TypeRef,
        without_function: bool,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        if !without_function {
            return Err(PgError::feature_not_supported(
                "CREATE CAST WITH FUNCTION / WITH INOUT is not supported yet",
            ));
        }
        if source == target {
            return Err(PgError::new(
                sqlstate::INVALID_OBJECT_DEFINITION,
                "source data type and target data type are the same",
            ));
        }
        let mut cat = self.inner.write().unwrap();

        // A cast for this type pair may only be defined once.
        if cat.casts.iter().any(|c| c.source == source && c.target == target) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!(
                    "cast from type {} to type {} already exists",
                    source.display_name(),
                    target.display_name()
                ),
            ));
        }

        // A binary-coercible cast requires physically identical representations;
        // PG checks typlen/typbyval/typalign, which for our types reduces to width.
        if let (Some(sl), Some(tl)) = (source.typlen(&cat), target.typlen(&cat))
            && sl != tl
        {
            return Err(PgError::new(
                sqlstate::INVALID_OBJECT_DEFINITION,
                "source and target data types are not physically compatible",
            ));
        }

        let oid = cat.alloc_oid();
        for tref in [&source, &target] {
            if let TypeRef::User(tname) = tref {
                cat.add_dependent(tname, DepId::Cast(oid));
            }
        }
        cat.casts.push(CastEntry {
            oid,
            source,
            target,
            without_function,
        });
        Ok(Vec::new())
    }

    /// `DROP TYPE a, b, ... [CASCADE|RESTRICT]`. Every target is validated before
    /// any is removed, so a failure on one name leaves the whole statement's
    /// targets intact — PG evaluates a multi-target DROP atomically. Reproduces
    /// PG's cascade NOTICE/DETAIL, the RESTRICT dependency error (with HINT), and
    /// the missing-type behaviour.
    pub fn drop_types(
        &self,
        names: &[&str],
        cascade: bool,
        if_exists: bool,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self.inner.write().unwrap();
        for name in names {
            cat.validate_drop_type(name, cascade, if_exists)?;
        }
        let mut notices = Vec::new();
        for name in names {
            notices.extend(cat.perform_drop_type(name));
        }
        Ok(notices)
    }

    /// `DROP CAST (source AS target) [CASCADE|RESTRICT]`.
    pub fn drop_cast(
        &self,
        source: TypeRef,
        target: TypeRef,
        if_exists: bool,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self.inner.write().unwrap();
        let found = cat
            .casts
            .iter()
            .find(|c| c.source == source && c.target == target)
            .map(|c| c.oid);
        match found {
            Some(oid) => {
                cat.remove_cast(oid);
                Ok(Vec::new())
            }
            None if if_exists => Ok(vec![CatalogNotice::new(format!(
                "cast from type {} to type {} does not exist, skipping",
                source.display_name(),
                target.display_name()
            ))]),
            None => Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!(
                    "cast from type {} to type {} does not exist",
                    source.display_name(),
                    target.display_name()
                ),
            )),
        }
    }
}

/// Query-time view: lets the binder resolve a user type name in a cast target
/// and apply a registered `WITHOUT FUNCTION` cast. Read-only (each method takes
/// a read lock); binding never holds a write lock, so there is no re-entrancy.
impl TypeCatalog for GlobalCatalog {
    fn resolve_type(&self, name: &str) -> Option<UserType> {
        let cat = self.inner.read().unwrap();
        cat.types.get(name).filter(|e| e.defined).map(|e| UserType {
            oid: e.oid,
            backing: e.backing,
        })
    }

    fn is_shell_type(&self, name: &str) -> bool {
        self.inner
            .read()
            .unwrap()
            .types
            .get(name)
            .is_some_and(|e| !e.defined)
    }

    fn user_type_name(&self, oid: u32) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .type_name_by_oid(oid)
            .map(str::to_string)
    }

    fn find_cast(&self, source: PgType, target: PgType) -> Option<UserCast> {
        let cat = self.inner.read().unwrap();
        let source = cat.type_ref_of(source)?;
        let target = cat.type_ref_of(target)?;
        cat.casts
            .iter()
            .find(|c| c.source == source && c.target == target)
            .map(|c| UserCast {
                without_function: c.without_function,
            })
    }

    fn backing_rep(&self, ty: PgType) -> PgType {
        match ty {
            PgType::User(oid) => {
                let cat = self.inner.read().unwrap();
                cat.types
                    .values()
                    .find(|e| e.oid == oid)
                    .and_then(|e| e.backing)
                    .unwrap_or(ty)
            }
            other => other,
        }
    }

    fn enum_info(&self, oid: u32) -> Option<EnumInfo> {
        let cat = self.inner.read().unwrap();
        let (name, entry) = cat.types.iter().find(|(_, e)| e.oid == oid)?;
        entry.enum_labels.as_ref().map(|labels| EnumInfo {
            name: name.clone(),
            labels: labels.clone(),
        })
    }
}

/// A user-defined type as surfaced to the system-catalog (`pg_type`/`pg_enum`)
/// introspection layer: its OID, name, and — when it is an enum — its labels in
/// definition order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserTypeInfo {
    pub oid: u32,
    pub name: String,
    pub enum_labels: Option<Vec<String>>,
}

impl GlobalCatalog {
    /// Every registered user type, for `pg_type`/`pg_enum` reflection. Shell
    /// types (not yet defined) are excluded, matching PG's `typisdefined`.
    pub fn user_types(&self) -> Vec<UserTypeInfo> {
        let cat = self.inner.read().unwrap();
        let mut types: Vec<_> = cat.types
            .iter()
            .filter(|(_, e)| e.defined)
            .map(|(name, e)| UserTypeInfo {
                oid: e.oid,
                name: name.clone(),
                enum_labels: e.enum_labels.clone(),
            })
            .collect();
        types.sort_by_key(|t| t.oid);
        types
    }
}

impl CatalogInner {
    /// Check that `name` can be dropped, without mutating: errors on a missing
    /// type (unless `if_exists`), or — under RESTRICT — on remaining dependents.
    fn validate_drop_type(
        &self,
        name: &str,
        cascade: bool,
        if_exists: bool,
    ) -> Result<(), PgError> {
        let Some(entry) = self.types.get(name) else {
            if if_exists {
                return Ok(());
            }
            return Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("type \"{name}\" does not exist"),
            ));
        };
        if !entry.dependents.is_empty() && !cascade {
            let lines: Vec<String> = entry
                .dependents
                .iter()
                .map(|dep| format!("{} depends on type {name}", self.describe_dep(*dep)))
                .collect();
            return Err(PgError::new(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop type {name} because other objects depend on it"),
            )
            .with_detail(lines.join("\n"))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        Ok(())
    }

    /// Remove a (previously validated) type and its dependents, returning the
    /// cascade or missing-type NOTICE(s). For a single dependent PG names it
    /// inline with no count/DETAIL; for several it prints the count plus a DETAIL
    /// block — reproduce both.
    fn perform_drop_type(&mut self, name: &str) -> Vec<CatalogNotice> {
        let Some(entry) = self.types.get(name) else {
            // Validated with `if_exists`: the type is simply absent.
            return vec![CatalogNotice::new(format!(
                "type \"{name}\" does not exist, skipping"
            ))];
        };
        let dependents = entry.dependents.clone();
        let notices = match dependents.as_slice() {
            [] => Vec::new(),
            [dep] => vec![CatalogNotice::new(format!(
                "drop cascades to {}",
                self.describe_dep(*dep)
            ))],
            deps => {
                let lines: Vec<String> = deps
                    .iter()
                    .map(|dep| format!("drop cascades to {}", self.describe_dep(*dep)))
                    .collect();
                vec![
                    CatalogNotice::new(format!("drop cascades to {} other objects", deps.len()))
                        .with_detail(lines.join("\n")),
                ]
            }
        };
        for dep in &dependents {
            match dep {
                DepId::Func(oid) => self.remove_func(*oid),
                DepId::Cast(oid) => self.remove_cast(*oid),
            }
        }
        self.types.remove(name);
        notices
    }

    fn describe_dep(&self, dep: DepId) -> String {
        match dep {
            DepId::Func(oid) => self
                .func(oid)
                .map(FuncEntry::describe)
                .unwrap_or_else(|| "function ?".to_string()),
            DepId::Cast(oid) => self
                .cast(oid)
                .map(CastEntry::describe)
                .unwrap_or_else(|| "cast ?".to_string()),
        }
    }

    fn remove_func(&mut self, oid: u32) {
        self.funcs.retain(|f| f.oid != oid);
        for entry in self.types.values_mut() {
            entry.dependents.retain(|d| *d != DepId::Func(oid));
        }
    }

    fn remove_cast(&mut self, oid: u32) {
        self.casts.retain(|c| c.oid != oid);
        for entry in self.types.values_mut() {
            entry.dependents.retain(|d| *d != DepId::Cast(oid));
        }
    }
}

/// The `LANGUAGE internal` built-in functions crabgresql recognises: the text
/// I/O functions of every scalar type crabgresql supports, so a user type can be
/// bootstrapped over any of them (as PG allows). PG has hundreds more; an
/// unrecognised name draws PG's "there is no built-in function named" error, as
/// it would in PG. Keep this in step with the supported `PgType` set.
fn is_known_internal(name: &str) -> bool {
    matches!(
        name,
        // integer / floating-point / numeric / boolean / bytea
        "int2in" | "int2out"
            | "int4in" | "int4out"
            | "int8in" | "int8out"
            | "float4in" | "float4out"
            | "float8in" | "float8out"
            | "numeric_in" | "numeric_out"
            | "boolin" | "boolout"
            | "byteain" | "byteaout"
            // character strings
            | "textin" | "textout"
            | "varcharin" | "varcharout"
            | "bpcharin" | "bpcharout"
            | "namein" | "nameout"
            | "bit_in" | "bit_out"
            // date / time
            | "date_in" | "date_out"
            | "time_in" | "time_out"
            | "timetz_in" | "timetz_out"
            | "timestamp_in" | "timestamp_out"
            | "timestamptz_in" | "timestamptz_out"
            | "interval_in" | "interval_out"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bootstrap the `xfloat8`-over-`int8` sequence and assert the
    /// `DROP TYPE ... CASCADE` NOTICE/DETAIL matches PostgreSQL.
    fn bootstrap_xfloat8() -> GlobalCatalog {
        let cat = GlobalCatalog::new();
        cat.create_shell_type("xfloat8").unwrap();
        let in_notices = cat
            .create_function(
                "xfloat8in",
                vec![(TypeRef::Cstring, None)],
                TypeRef::User("xfloat8".into()),
                "int8in",
            )
            .unwrap();
        assert_eq!(in_notices[0].message, "return type xfloat8 is only a shell");
        let out_notices = cat
            .create_function(
                "xfloat8out",
                vec![(TypeRef::User("xfloat8".into()), Some((1, 28)))],
                TypeRef::Cstring,
                "int8out",
            )
            .unwrap();
        assert_eq!(
            out_notices[0].message,
            "argument type xfloat8 is only a shell"
        );
        // The argument NOTICE carries the type token's (line, column) for the
        // client's `LINE n:` caret; the return-type NOTICE (above) does not.
        assert_eq!(out_notices[0].position, Some((1, 28)));
        cat.define_type("xfloat8", 8, Some(PgType::Int8)).unwrap();
        cat.create_cast(
            TypeRef::User("xfloat8".into()),
            TypeRef::Builtin(PgType::Int8),
            true,
        )
        .unwrap();
        cat
    }

    #[test]
    fn drop_cascade_lists_dependents_in_creation_order() {
        let cat = bootstrap_xfloat8();
        let notices = cat.drop_types(&["xfloat8"], true, false).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].message, "drop cascades to 3 other objects");
        assert_eq!(
            notices[0].detail.as_deref().unwrap(),
            "drop cascades to function xfloat8in(cstring)\n\
             drop cascades to function xfloat8out(xfloat8)\n\
             drop cascades to cast from xfloat8 to bigint"
        );
        // The type and its dependents are gone.
        assert!(!cat.is_user_type("xfloat8"));
    }

    #[test]
    fn drop_cascade_single_dependent_has_no_count_or_detail() {
        // Exactly one dependent: PG names it inline with no count and no DETAIL.
        let cat = GlobalCatalog::new();
        cat.create_shell_type("solo").unwrap();
        cat.create_function(
            "solo_in",
            vec![(TypeRef::Cstring, None)],
            TypeRef::User("solo".into()),
            "int8in",
        )
        .unwrap();
        let notices = cat.drop_types(&["solo"], true, false).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].message,
            "drop cascades to function solo_in(cstring)"
        );
        assert!(notices[0].detail.is_none());
    }

    #[test]
    fn drop_restrict_refuses_with_hint() {
        let cat = bootstrap_xfloat8();
        let err = cat.drop_types(&["xfloat8"], false, false).unwrap_err();
        assert_eq!(err.code, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(
            err.message,
            "cannot drop type xfloat8 because other objects depend on it"
        );
        assert_eq!(
            err.detail.as_deref().unwrap(),
            "function xfloat8in(cstring) depends on type xfloat8\n\
             function xfloat8out(xfloat8) depends on type xfloat8\n\
             cast from xfloat8 to bigint depends on type xfloat8"
        );
        assert!(err.hint.is_some());
        // Nothing was dropped.
        assert!(cat.is_user_type("xfloat8"));
    }

    #[test]
    fn multi_target_drop_is_atomic() {
        // `DROP TYPE xfloat8, nope;` must leave xfloat8 intact when a later name
        // does not exist — PG evaluates the whole statement atomically.
        let cat = bootstrap_xfloat8();
        let err = cat.drop_types(&["xfloat8", "nope"], true, false).unwrap_err();
        assert_eq!(err.code, sqlstate::UNDEFINED_OBJECT);
        assert!(cat.is_user_type("xfloat8"));
    }

    #[test]
    fn redeclaring_a_type_is_a_duplicate_error() {
        let cat = GlobalCatalog::new();
        cat.create_shell_type("foo").unwrap();
        let err = cat.create_shell_type("foo").unwrap_err();
        assert_eq!(err.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(err.message, "type \"foo\" already exists");
    }

    #[test]
    fn shell_types_do_not_resolve_for_queries() {
        let cat = GlobalCatalog::new();
        cat.create_shell_type("foo").unwrap();
        assert!(cat.is_shell_type("foo"));
        assert!(cat.resolve_type("foo").is_none());
        cat.define_type("foo", 8, Some(PgType::Int8)).unwrap();
        assert!(!cat.is_shell_type("foo"));
        assert!(cat.resolve_type("foo").is_some());
    }

    #[test]
    fn user_type_enumeration_follows_oid_order() {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("zeta", vec!["z".into()]).unwrap();
        cat.create_enum_type("alpha", vec!["a".into()]).unwrap();
        let names: Vec<_> = cat.user_types().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["zeta", "alpha"]);
    }

    #[test]
    fn duplicate_cast_is_rejected() {
        let cat = bootstrap_xfloat8();
        let err = cat
            .create_cast(
                TypeRef::User("xfloat8".into()),
                TypeRef::Builtin(PgType::Int8),
                true,
            )
            .unwrap_err();
        assert_eq!(err.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(
            err.message,
            "cast from type xfloat8 to type bigint already exists"
        );
    }

    #[test]
    fn self_cast_is_rejected() {
        let cat = GlobalCatalog::new();
        let err = cat
            .create_cast(
                TypeRef::Builtin(PgType::Int8),
                TypeRef::Builtin(PgType::Int8),
                true,
            )
            .unwrap_err();
        assert_eq!(err.code, sqlstate::INVALID_OBJECT_DEFINITION);
        assert_eq!(
            err.message,
            "source data type and target data type are the same"
        );
    }

    #[test]
    fn unknown_internal_function_is_rejected() {
        let cat = GlobalCatalog::new();
        let err = cat
            .create_function("f", vec![(TypeRef::Cstring, None)], TypeRef::Builtin(PgType::Int8), "nope")
            .unwrap_err();
        assert_eq!(err.code, sqlstate::UNDEFINED_FUNCTION);
        assert_eq!(err.message, "there is no built-in function named \"nope\"");
    }

    #[test]
    fn date_io_internals_are_accepted() {
        // A type can be bootstrapped over any supported type's I/O internals.
        let cat = GlobalCatalog::new();
        cat.create_shell_type("xd").unwrap();
        assert!(
            cat.create_function(
                "xd_in",
                vec![(TypeRef::Cstring, None)],
                TypeRef::User("xd".into()),
                "date_in",
            )
            .is_ok()
        );
    }

    #[test]
    fn without_function_cast_requires_matching_width() {
        let cat = GlobalCatalog::new();
        let err = cat
            .create_cast(
                TypeRef::Builtin(PgType::Int2),
                TypeRef::Builtin(PgType::Int8),
                true,
            )
            .unwrap_err();
        assert_eq!(err.code, sqlstate::INVALID_OBJECT_DEFINITION);
    }

    #[test]
    fn create_enum_type_and_enum_info_roundtrip() {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("rainbow", vec!["red".into(), "green".into(), "blue".into()])
            .unwrap();
        assert!(cat.is_user_type("rainbow"));
        let oid = cat.resolve_type("rainbow").unwrap().oid;
        let info = cat.enum_info(oid).unwrap();
        assert_eq!(info.name, "rainbow");
        assert_eq!(info.labels, vec!["red", "green", "blue"]);
        // A non-enum OID (or a base user type) has no enum_info.
        assert!(cat.enum_info(oid + 1).is_none());
        // Surfaced to the pg_type/pg_enum reflection layer.
        let rows = cat.user_types();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "rainbow");
        assert_eq!(
            rows[0].enum_labels.as_deref(),
            Some(&["red".to_string(), "green".to_string(), "blue".to_string()][..])
        );
    }

    #[test]
    fn create_enum_duplicate_label_rejected() {
        let cat = GlobalCatalog::new();
        let err = cat
            .create_enum_type("dup", vec!["a".into(), "b".into(), "a".into()])
            .unwrap_err();
        assert_eq!(err.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(err.message, "enum label \"a\" used more than once");
        // A rejected definition registers nothing.
        assert!(!cat.is_user_type("dup"));
    }

    #[test]
    fn create_enum_duplicate_name_rejected() {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("e", vec!["a".into()]).unwrap();
        let err = cat.create_enum_type("e", vec!["b".into()]).unwrap_err();
        assert_eq!(err.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(err.message, "type \"e\" already exists");
    }

    #[test]
    fn missing_type_drop_reports_error_or_skips() {
        let cat = GlobalCatalog::new();
        assert!(cat.drop_types(&["nope"], false, false).is_err());
        // IF EXISTS turns the miss into a skip NOTICE.
        let notices = cat.drop_types(&["nope"], false, true).unwrap();
        assert_eq!(notices[0].message, "type \"nope\" does not exist, skipping");
    }
}
