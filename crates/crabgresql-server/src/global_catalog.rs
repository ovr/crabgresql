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
use crabgresql_types::PgType;

use crate::error::PgError;

/// PG's `FirstNormalObjectId`: user object OIDs are allocated from here up.
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
    /// The name PG uses for this type in dependency / cascade messages
    /// (`format_type_be`): `bigint` for int8, the type name for a user type.
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
}

impl CatalogNotice {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
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
    #[allow(dead_code)]
    oid: u32,
    /// `false` while the type is a bare shell (`CREATE TYPE name;`).
    defined: bool,
    /// Physical width (`pg_type.typlen`); -1 for variable-length.
    typlen: i32,
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

    /// `CREATE TYPE name;` — register a shell type. A redefinition of an existing
    /// shell is a no-op; a name already fully defined is a duplicate error.
    pub fn create_shell_type(&self, name: &str) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self.inner.write().unwrap();
        if let Some(existing) = cat.types.get(name) {
            if existing.defined {
                return Err(PgError::new(
                    sqlstate::DUPLICATE_OBJECT,
                    format!("type \"{name}\" already exists"),
                ));
            }
            // Re-declaring a shell is harmless.
            return Ok(Vec::new());
        }
        let oid = cat.alloc_oid();
        cat.types.insert(
            name.to_string(),
            TypeEntry {
                oid,
                defined: false,
                typlen: -1,
                dependents: Vec::new(),
            },
        );
        Ok(Vec::new())
    }

    /// `CREATE TYPE name (...)` — fill in a previously declared shell (or create
    /// a defined type directly). `typlen` is derived from the LIKE/INTERNALLENGTH
    /// options by the caller.
    pub fn define_type(&self, name: &str, typlen: i32) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self.inner.write().unwrap();
        match cat.types.get_mut(name) {
            Some(entry) if entry.defined => Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("type \"{name}\" already exists"),
            )),
            Some(entry) => {
                entry.defined = true;
                entry.typlen = typlen;
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
                        dependents: Vec::new(),
                    },
                );
                Ok(Vec::new())
            }
        }
    }

    /// `CREATE FUNCTION ... LANGUAGE internal AS '<internal_name>'`. Validates the
    /// internal name, registers the function, records its type dependencies, and
    /// returns any "argument/return type is only a shell" NOTICEs.
    pub fn create_function(
        &self,
        name: &str,
        args: Vec<TypeRef>,
        ret: TypeRef,
        internal_name: &str,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        if !is_known_internal(internal_name) {
            return Err(PgError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!("there is no built-in function named \"{internal_name}\""),
            ));
        }

        let mut cat = self.inner.write().unwrap();
        let mut notices = Vec::new();

        // PG reports a shell reference for the return type first, then arguments.
        if let TypeRef::User(tname) = &ret
            && cat.types.get(tname).is_some_and(|e| !e.defined)
        {
            notices.push(CatalogNotice::new(format!(
                "return type {tname} is only a shell"
            )));
        }
        for arg in &args {
            if let TypeRef::User(tname) = arg
                && cat.types.get(tname).is_some_and(|e| !e.defined)
            {
                notices.push(CatalogNotice::new(format!(
                    "argument type {tname} is only a shell"
                )));
            }
        }

        let oid = cat.alloc_oid();
        // Every user type mentioned in the signature depends on this function.
        for tref in std::iter::once(&ret).chain(args.iter()) {
            if let TypeRef::User(tname) = tref {
                let tname = tname.clone();
                cat.add_dependent(&tname, DepId::Func(oid));
            }
        }
        cat.funcs.push(FuncEntry {
            oid,
            name: name.to_string(),
            args,
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
        let mut cat = self.inner.write().unwrap();

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
                let tname = tname.clone();
                cat.add_dependent(&tname, DepId::Cast(oid));
            }
        }
        cat.casts.push(CastEntry {
            oid,
            source,
            target,
        });
        Ok(Vec::new())
    }

    /// `DROP TYPE name [CASCADE|RESTRICT]`. Reproduces PG's cascade NOTICE/DETAIL,
    /// the RESTRICT dependency error (with HINT), and the missing-type behaviour.
    pub fn drop_type(
        &self,
        name: &str,
        cascade: bool,
        if_exists: bool,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self.inner.write().unwrap();
        let Some(entry) = cat.types.get(name) else {
            if if_exists {
                return Ok(vec![CatalogNotice::new(format!(
                    "type \"{name}\" does not exist, skipping"
                ))]);
            }
            return Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("type \"{name}\" does not exist"),
            ));
        };

        let dependents = entry.dependents.clone();
        if !dependents.is_empty() && !cascade {
            // RESTRICT (the default): refuse, listing each dependent.
            let lines: Vec<String> = dependents
                .iter()
                .map(|dep| format!("{} depends on type {name}", cat.describe_dep(*dep)))
                .collect();
            return Err(PgError::new(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop type {name} because other objects depend on it"),
            )
            .with_detail(lines.join("\n"))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }

        // Collect the cascade description before removing anything.
        let notices = if dependents.is_empty() {
            Vec::new()
        } else {
            let lines: Vec<String> = dependents
                .iter()
                .map(|dep| format!("drop cascades to {}", cat.describe_dep(*dep)))
                .collect();
            let n = dependents.len();
            vec![
                CatalogNotice::new(format!("drop cascades to {n} other objects"))
                    .with_detail(lines.join("\n")),
            ]
        };

        // Remove the dependents, then the type itself.
        for dep in &dependents {
            match dep {
                DepId::Func(oid) => cat.remove_func(*oid),
                DepId::Cast(oid) => cat.remove_cast(*oid),
            }
        }
        cat.types.remove(name);
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
                "cast from {} to {} does not exist, skipping",
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

impl CatalogInner {
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

/// The `LANGUAGE internal` built-in functions crabgresql recognises. PG has
/// hundreds; this is the curated subset needed to bootstrap the supported user
/// types (chiefly the integer/text I/O functions). An unrecognised name draws
/// PG's "there is no built-in function named" error, as it would in PG.
fn is_known_internal(name: &str) -> bool {
    matches!(
        name,
        "int2in"
            | "int2out"
            | "int4in"
            | "int4out"
            | "int8in"
            | "int8out"
            | "float4in"
            | "float4out"
            | "float8in"
            | "float8out"
            | "textin"
            | "textout"
            | "boolin"
            | "boolout"
            | "byteain"
            | "byteaout"
            | "numeric_in"
            | "numeric_out"
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
                vec![TypeRef::Cstring],
                TypeRef::User("xfloat8".into()),
                "int8in",
            )
            .unwrap();
        assert_eq!(in_notices[0].message, "return type xfloat8 is only a shell");
        let out_notices = cat
            .create_function(
                "xfloat8out",
                vec![TypeRef::User("xfloat8".into())],
                TypeRef::Cstring,
                "int8out",
            )
            .unwrap();
        assert_eq!(
            out_notices[0].message,
            "argument type xfloat8 is only a shell"
        );
        cat.define_type("xfloat8", 8).unwrap();
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
        let notices = cat.drop_type("xfloat8", true, false).unwrap();
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
    fn drop_restrict_refuses_with_hint() {
        let cat = bootstrap_xfloat8();
        let err = cat.drop_type("xfloat8", false, false).unwrap_err();
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
    fn unknown_internal_function_is_rejected() {
        let cat = GlobalCatalog::new();
        let err = cat
            .create_function("f", vec![TypeRef::Cstring], TypeRef::Builtin(PgType::Int8), "nope")
            .unwrap_err();
        assert_eq!(err.code, sqlstate::UNDEFINED_FUNCTION);
        assert_eq!(err.message, "there is no built-in function named \"nope\"");
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
    fn missing_type_drop_reports_error_or_skips() {
        let cat = GlobalCatalog::new();
        assert!(cat.drop_type("nope", false, false).is_err());
        // IF EXISTS turns the miss into a skip NOTICE.
        let notices = cat.drop_type("nope", false, true).unwrap();
        assert_eq!(notices[0].message, "type \"nope\" does not exist, skipping");
    }
}
