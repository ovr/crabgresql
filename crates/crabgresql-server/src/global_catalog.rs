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

    /// Rewrite every stored `TypeRef::User(from)` to `TypeRef::User(to)` in the
    /// function and cast signatures. These reference user types by name (not
    /// OID), so a `ALTER TYPE ... RENAME TO` must carry the rename into them.
    fn rename_type_refs(&mut self, from: &str, to: &str) {
        fn rewrite(r: &mut TypeRef, from: &str, to: &str) {
            if let TypeRef::User(name) = r
                && name == from
            {
                *name = to.to_string();
            }
        }
        for func in &mut self.funcs {
            for arg in &mut func.args {
                rewrite(arg, from, to);
            }
        }
        for cast in &mut self.casts {
            rewrite(&mut cast.source, from, to);
            rewrite(&mut cast.target, from, to);
        }
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

/// The mutable label list of the enum type `ty`, or the error `ALTER TYPE ...
/// {ADD|RENAME} VALUE` raises when the type is missing (`42704`) or is not an
/// enum (`42809`).
fn enum_labels_mut<'a>(
    cat: &'a mut CatalogInner,
    ty: &str,
) -> Result<&'a mut Vec<String>, PgError> {
    match cat.types.get_mut(ty) {
        None => Err(PgError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("type \"{ty}\" does not exist"),
        )),
        // PG's `%s is not an enum` renders the type name unquoted (format_type_be).
        Some(entry) => entry.enum_labels.as_mut().ok_or_else(|| {
            PgError::new(sqlstate::WRONG_OBJECT_TYPE, format!("{ty} is not an enum"))
        }),
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
        self.inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .types
            .contains_key(name)
    }

    /// Physical width of a user type, for a `LIKE` clause referencing it.
    pub fn user_type_typlen(&self, name: &str) -> Option<i32> {
        self.inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .types
            .get(name)
            .map(|e| e.typlen)
    }

    /// Backing builtin of a user type, propagated when another type is `LIKE` it.
    pub fn user_type_backing(&self, name: &str) -> Option<PgType> {
        self.inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .types
            .get(name)
            .and_then(|e| e.backing)
    }

    /// `CREATE TYPE name;` — register a shell type. Re-declaring any existing
    /// type (shell or fully defined) is a duplicate-object error, as in PG.
    pub fn create_shell_type(&self, name: &str) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
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
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
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
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
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

    /// `ALTER TYPE old RENAME TO new`. Works for any user type (enum or base):
    /// only the `types` map key changes and the OID is preserved, so existing
    /// `pg_type`/`pg_enum` rows and stored `Value::Enum { type_oid }` stay valid.
    /// The existence of `old` is checked before the target-name collision, so a
    /// missing source reports "does not exist" (not the target's "already
    /// exists"), matching PG. A `new` that collides with a builtin name or an
    /// existing user type (including `old` itself) is a duplicate-object error.
    pub fn rename_type(&self, old: &str, new: &str) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if !cat.types.contains_key(old) {
            return Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("type \"{old}\" does not exist"),
            ));
        }
        if crabgresql_catalog::is_builtin_type_name(new) || cat.types.contains_key(new) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("type \"{new}\" already exists"),
            ));
        }
        // Function and cast signatures store their argument/source/target types
        // by name (`TypeRef::User`), not by OID, so the rename must follow the
        // name into them or those objects stop resolving against this type.
        cat.rename_type_refs(old, new);
        let entry = cat.types.remove(old).expect("type present after check");
        cat.types.insert(new.to_string(), entry);
        Ok(Vec::new())
    }

    /// `ALTER TYPE ty ADD VALUE [IF NOT EXISTS] 'value' [{BEFORE|AFTER} 'neighbor']`.
    /// `position` is `Some((before, neighbor))` where `before` selects BEFORE vs
    /// AFTER; `None` appends. Labels are stored verbatim (case-sensitive), like
    /// [`Self::create_enum_type`].
    ///
    /// Known limitation: an enum value's ordinal is baked into every stored
    /// `Value::Enum` at bind time, so inserting BEFORE/AFTER an existing label
    /// renumbers later labels for *new* values only — rows already materialized
    /// keep their old ordinal until re-read. PG avoids this with a fractional
    /// `enumsortorder`; matching that is out of scope. Appending (no position) is
    /// always safe.
    pub fn add_enum_value(
        &self,
        ty: &str,
        value: &str,
        if_not_exists: bool,
        position: Option<(bool, String)>,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let labels = enum_labels_mut(&mut cat, ty)?;
        if labels.iter().any(|l| l == value) {
            if if_not_exists {
                return Ok(vec![CatalogNotice::new(format!(
                    "enum label \"{value}\" already exists, skipping"
                ))]);
            }
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("enum label \"{value}\" already exists"),
            ));
        }
        let index = match &position {
            None => labels.len(),
            Some((before, neighbor)) => {
                let pos = labels.iter().position(|l| l == neighbor).ok_or_else(|| {
                    PgError::new(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format!("\"{neighbor}\" is not an existing enum label"),
                    )
                })?;
                if *before { pos } else { pos + 1 }
            }
        };
        labels.insert(index, value.to_string());
        Ok(Vec::new())
    }

    /// `ALTER TYPE ty RENAME VALUE 'from' TO 'to'`. Replaces the label in place,
    /// so its ordinal is unchanged. Rows already holding the old label string
    /// keep it until re-read (see [`Self::add_enum_value`] for the staleness note).
    pub fn rename_enum_value(
        &self,
        ty: &str,
        from: &str,
        to: &str,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let labels = enum_labels_mut(&mut cat, ty)?;
        // PG reports a missing source label before a colliding target one.
        let from_pos = labels.iter().position(|l| l == from).ok_or_else(|| {
            PgError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("\"{from}\" is not an existing enum label"),
            )
        })?;
        if labels.iter().any(|l| l == to) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("enum label \"{to}\" already exists"),
            ));
        }
        labels[from_pos] = to.to_string();
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

        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));

        // A function may not be redefined with the same name and argument types.
        if cat
            .funcs
            .iter()
            .any(|f| f.name == name && f.args == arg_types)
        {
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
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));

        // A cast for this type pair may only be defined once.
        if cat
            .casts
            .iter()
            .any(|c| c.source == source && c.target == target)
        {
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
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
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
        let mut cat = self
            .inner
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
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
        let cat = self
            .inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        cat.types.get(name).filter(|e| e.defined).map(|e| UserType {
            oid: e.oid,
            backing: e.backing,
        })
    }

    fn is_shell_type(&self, name: &str) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .types
            .get(name)
            .is_some_and(|e| !e.defined)
    }

    fn user_type_name(&self, oid: u32) -> Option<String> {
        self.inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .type_name_by_oid(oid)
            .map(str::to_string)
    }

    fn find_cast(&self, source: PgType, target: PgType) -> Option<UserCast> {
        let cat = self
            .inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
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
                let cat = self
                    .inner
                    .read()
                    .unwrap_or_else(|_| panic!("rwlock poisoned"));
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
        let cat = self
            .inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
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
        let cat = self
            .inner
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Bootstrap the `xfloat8`-over-`int8` sequence and assert the
    /// `DROP TYPE ... CASCADE` NOTICE/DETAIL matches PostgreSQL.
    fn bootstrap_xfloat8() -> GlobalCatalog {
        let cat = GlobalCatalog::new();
        if let Err(error) = cat.create_shell_type("xfloat8") {
            panic!("failed to create shell type test fixture: {error}");
        }
        let in_notices = match cat.create_function(
            "xfloat8in",
            vec![(TypeRef::Cstring, None)],
            TypeRef::User("xfloat8".into()),
            "int8in",
        ) {
            Ok(notices) => notices,
            Err(error) => panic!("failed to create input function test fixture: {error}"),
        };
        assert_eq!(in_notices[0].message, "return type xfloat8 is only a shell");
        let out_notices = match cat.create_function(
            "xfloat8out",
            vec![(TypeRef::User("xfloat8".into()), Some((1, 28)))],
            TypeRef::Cstring,
            "int8out",
        ) {
            Ok(notices) => notices,
            Err(error) => panic!("failed to create output function test fixture: {error}"),
        };
        assert_eq!(
            out_notices[0].message,
            "argument type xfloat8 is only a shell"
        );
        // The argument NOTICE carries the type token's (line, column) for the
        // client's `LINE n:` caret; the return-type NOTICE (above) does not.
        assert_eq!(out_notices[0].position, Some((1, 28)));
        if let Err(error) = cat.define_type("xfloat8", 8, Some(PgType::Int8)) {
            panic!("failed to define type test fixture: {error}");
        }
        if let Err(error) = cat.create_cast(
            TypeRef::User("xfloat8".into()),
            TypeRef::Builtin(PgType::Int8),
            true,
        ) {
            panic!("failed to create cast test fixture: {error}");
        }
        cat
    }

    #[test]
    fn drop_cascade_lists_dependents_in_creation_order() -> anyhow::Result<()> {
        let cat = bootstrap_xfloat8();
        let notices = cat.drop_types(&["xfloat8"], true, false)?;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].message, "drop cascades to 3 other objects");
        assert_eq!(
            notices[0]
                .detail
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("cascade notice detail is missing"))?,
            "drop cascades to function xfloat8in(cstring)\n\
             drop cascades to function xfloat8out(xfloat8)\n\
             drop cascades to cast from xfloat8 to bigint"
        );
        // The type and its dependents are gone.
        assert!(!cat.is_user_type("xfloat8"));

        Ok(())
    }

    #[test]
    fn drop_cascade_single_dependent_has_no_count_or_detail() -> anyhow::Result<()> {
        // Exactly one dependent: PG names it inline with no count and no DETAIL.
        let cat = GlobalCatalog::new();
        cat.create_shell_type("solo")?;
        cat.create_function(
            "solo_in",
            vec![(TypeRef::Cstring, None)],
            TypeRef::User("solo".into()),
            "int8in",
        )?;
        let notices = cat.drop_types(&["solo"], true, false)?;
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].message,
            "drop cascades to function solo_in(cstring)"
        );
        assert!(notices[0].detail.is_none());

        Ok(())
    }

    #[test]
    fn drop_restrict_refuses_with_hint() -> anyhow::Result<()> {
        let cat = bootstrap_xfloat8();
        let err = cat.drop_types(&["xfloat8"], false, false).unwrap_err();
        assert_eq!(err.code, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(
            err.message,
            "cannot drop type xfloat8 because other objects depend on it"
        );
        assert_eq!(
            err.detail
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("dependency detail is missing"))?,
            "function xfloat8in(cstring) depends on type xfloat8\n\
             function xfloat8out(xfloat8) depends on type xfloat8\n\
             cast from xfloat8 to bigint depends on type xfloat8"
        );
        assert!(err.hint.is_some());
        // Nothing was dropped.
        assert!(cat.is_user_type("xfloat8"));

        Ok(())
    }

    #[test]
    fn multi_target_drop_is_atomic() {
        // `DROP TYPE xfloat8, nope;` must leave xfloat8 intact when a later name
        // does not exist — PG evaluates the whole statement atomically.
        let cat = bootstrap_xfloat8();
        let err = cat
            .drop_types(&["xfloat8", "nope"], true, false)
            .unwrap_err();
        assert_eq!(err.code, sqlstate::UNDEFINED_OBJECT);
        assert!(cat.is_user_type("xfloat8"));
    }

    #[test]
    fn redeclaring_a_type_is_a_duplicate_error() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_shell_type("foo")?;
        let err = cat.create_shell_type("foo").unwrap_err();
        assert_eq!(err.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(err.message, "type \"foo\" already exists");

        Ok(())
    }

    #[test]
    fn shell_types_do_not_resolve_for_queries() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_shell_type("foo")?;
        assert!(cat.is_shell_type("foo"));
        assert!(cat.resolve_type("foo").is_none());
        cat.define_type("foo", 8, Some(PgType::Int8))?;
        assert!(!cat.is_shell_type("foo"));
        assert!(cat.resolve_type("foo").is_some());

        Ok(())
    }

    #[test]
    fn user_type_enumeration_follows_oid_order() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("zeta", vec!["z".into()])?;
        cat.create_enum_type("alpha", vec!["a".into()])?;
        let names: Vec<_> = cat.user_types().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["zeta", "alpha"]);

        Ok(())
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
            .create_function(
                "f",
                vec![(TypeRef::Cstring, None)],
                TypeRef::Builtin(PgType::Int8),
                "nope",
            )
            .unwrap_err();
        assert_eq!(err.code, sqlstate::UNDEFINED_FUNCTION);
        assert_eq!(err.message, "there is no built-in function named \"nope\"");
    }

    #[test]
    fn date_io_internals_are_accepted() -> anyhow::Result<()> {
        // A type can be bootstrapped over any supported type's I/O internals.
        let cat = GlobalCatalog::new();
        cat.create_shell_type("xd")?;
        assert!(
            cat.create_function(
                "xd_in",
                vec![(TypeRef::Cstring, None)],
                TypeRef::User("xd".into()),
                "date_in",
            )
            .is_ok()
        );

        Ok(())
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
    fn create_enum_type_and_enum_info_roundtrip() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("rainbow", vec!["red".into(), "green".into(), "blue".into()])?;
        assert!(cat.is_user_type("rainbow"));
        let oid = cat.resolve_type("rainbow").expect("rainbow resolves").oid;
        let info = cat.enum_info(oid).expect("rainbow has enum info");
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

        Ok(())
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
    fn create_enum_duplicate_name_rejected() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("e", vec!["a".into()])?;
        let err = cat.create_enum_type("e", vec!["b".into()]).unwrap_err();
        assert_eq!(err.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(err.message, "type \"e\" already exists");

        Ok(())
    }

    #[test]
    fn missing_type_drop_reports_error_or_skips() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        assert!(cat.drop_types(&["nope"], false, false).is_err());
        // IF EXISTS turns the miss into a skip NOTICE.
        let notices = cat.drop_types(&["nope"], false, true)?;
        assert_eq!(notices[0].message, "type \"nope\" does not exist, skipping");

        Ok(())
    }

    #[test]
    fn rename_type_preserves_oid_and_moves_key() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("rainbow", vec!["red".into(), "green".into()])?;
        let oid = cat.resolve_type("rainbow").expect("resolves").oid;
        cat.rename_type("rainbow", "colors")?;
        assert!(!cat.is_user_type("rainbow"));
        assert!(cat.is_user_type("colors"));
        // Same OID under the new name, so pg_enum/Value::Enum stay valid.
        assert_eq!(cat.resolve_type("colors").expect("resolves").oid, oid);
        let info = cat.enum_info(oid).expect("enum info follows the rename");
        assert_eq!(info.name, "colors");
        assert_eq!(info.labels, vec!["red", "green"]);

        Ok(())
    }

    #[test]
    fn rename_type_errors_on_missing_and_collision() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("a", vec!["x".into()])?;
        cat.create_enum_type("b", vec!["y".into()])?;
        let missing = cat.rename_type("nope", "z").unwrap_err();
        assert_eq!(missing.code, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(missing.message, "type \"nope\" does not exist");
        let collide = cat.rename_type("a", "b").unwrap_err();
        assert_eq!(collide.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(collide.message, "type \"b\" already exists");
        // A target that collides with a builtin name is a duplicate-object error.
        let builtin = cat.rename_type("a", "int4").unwrap_err();
        assert_eq!(builtin.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(builtin.message, "type \"int4\" already exists");
        // Source existence is checked before the target collision: a missing
        // source with a builtin target still reports the source, not the target.
        let order = cat.rename_type("nope", "int4").unwrap_err();
        assert_eq!(order.code, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(order.message, "type \"nope\" does not exist");

        Ok(())
    }

    #[test]
    fn rename_type_follows_the_name_into_cast_signatures() -> anyhow::Result<()> {
        // Casts store their source/target by name (TypeRef::User), so a rename
        // must carry into them or find_cast (keyed on the current OID->name) stops
        // matching a cast created under the old name.
        let cat = GlobalCatalog::new();
        cat.define_type("t", 4, Some(PgType::Int4))?;
        let oid = cat.resolve_type("t").expect("resolves").oid;
        cat.create_cast(
            TypeRef::User("t".into()),
            TypeRef::Builtin(PgType::Int4),
            true,
        )?;
        assert!(cat.find_cast(PgType::User(oid), PgType::Int4).is_some());
        cat.rename_type("t", "u")?;
        // Same OID, and the cast still resolves after the rename.
        assert_eq!(cat.resolve_type("u").expect("resolves").oid, oid);
        assert!(cat.find_cast(PgType::User(oid), PgType::Int4).is_some());

        Ok(())
    }

    #[test]
    fn add_enum_value_append_before_after() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("e", vec!["b".into(), "d".into()])?;
        let oid = cat.resolve_type("e").expect("resolves").oid;
        cat.add_enum_value("e", "e", false, None)?; // append
        cat.add_enum_value("e", "a", false, Some((true, "b".into())))?; // before b
        cat.add_enum_value("e", "c", false, Some((false, "b".into())))?; // after b
        let info = cat.enum_info(oid).expect("enum info");
        assert_eq!(info.labels, vec!["a", "b", "c", "d", "e"]);

        Ok(())
    }

    #[test]
    fn add_enum_value_duplicate_and_if_not_exists() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("e", vec!["a".into()])?;
        let err = cat.add_enum_value("e", "a", false, None).unwrap_err();
        assert_eq!(err.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(err.message, "enum label \"a\" already exists");
        // IF NOT EXISTS turns the duplicate into a skip NOTICE, no mutation.
        let notices = cat.add_enum_value("e", "a", true, None)?;
        assert_eq!(
            notices[0].message,
            "enum label \"a\" already exists, skipping"
        );
        let oid = cat.resolve_type("e").expect("resolves").oid;
        assert_eq!(cat.enum_info(oid).expect("info").labels, vec!["a"]);

        Ok(())
    }

    #[test]
    fn add_enum_value_errors_on_missing_neighbor_and_non_enum() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("e", vec!["a".into()])?;
        let neighbor = cat
            .add_enum_value("e", "b", false, Some((false, "zzz".into())))
            .unwrap_err();
        assert_eq!(neighbor.code, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(neighbor.message, "\"zzz\" is not an existing enum label");
        // A base (non-enum) user type rejects ADD VALUE, unquoted name.
        cat.define_type("base", 4, None)?;
        let non_enum = cat.add_enum_value("base", "x", false, None).unwrap_err();
        assert_eq!(non_enum.code, sqlstate::WRONG_OBJECT_TYPE);
        assert_eq!(non_enum.message, "base is not an enum");
        // A missing type reports undefined-object with the quoted name.
        let missing = cat.add_enum_value("nope", "x", false, None).unwrap_err();
        assert_eq!(missing.code, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(missing.message, "type \"nope\" does not exist");

        Ok(())
    }

    #[test]
    fn rename_enum_value_in_place_and_errors() -> anyhow::Result<()> {
        let cat = GlobalCatalog::new();
        cat.create_enum_type("e", vec!["red".into(), "green".into(), "blue".into()])?;
        let oid = cat.resolve_type("e").expect("resolves").oid;
        cat.rename_enum_value("e", "red", "crimson")?;
        // Renamed in place: ordinal/position unchanged.
        assert_eq!(
            cat.enum_info(oid).expect("info").labels,
            vec!["crimson", "green", "blue"]
        );
        // Missing source label is reported before a colliding target.
        let missing = cat.rename_enum_value("e", "red", "green").unwrap_err();
        assert_eq!(missing.code, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(missing.message, "\"red\" is not an existing enum label");
        let collide = cat.rename_enum_value("e", "blue", "green").unwrap_err();
        assert_eq!(collide.code, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(collide.message, "enum label \"green\" already exists");

        Ok(())
    }
}
