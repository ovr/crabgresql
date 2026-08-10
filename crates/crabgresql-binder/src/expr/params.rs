//! Per-statement parameter state: the `$n` slots a statement's binds share,
//! and the view-expansion bookkeeping that rides along with them.

use crabgresql_types::PgType;

use crate::BindError;

/// Shared, mutable bind state for one statement. A `$n` occurrence anywhere in
/// the statement — target list, WHERE, a subquery, a CTE — refers to the same
/// slot here, so a type deduced at one site is visible at every other. Held
/// behind `Rc<RefCell<…>>` because the binder threads a single context through
/// the whole tree (see [`crate::Scope`]) while several sites borrow it. That
/// reach is
/// also why the view-expansion state rides along: it has to be visible at every
/// relation reference, including one nested inside an expression subquery.
pub type ParamCtx = std::rc::Rc<std::cell::RefCell<ParamState>>;

/// A view's identity while it is being expanded: `(namespace, name)`. Both come
/// from the *resolved* view definition, not from how the reference was spelled,
/// so the key is unambiguous across schemas.
pub(super) type ViewKey = (String, String);

/// The views whose bodies are currently being bound (outermost first) and how
/// many expansions this statement has done. Behind its own `Rc` so a nested view
/// body can share *this* while getting a fresh `$n` namespace — see
/// [`param_ctx_view_body`]. The binder's `ViewExpansionGuard` owns the policy.
pub(crate) type ViewExpansion = std::rc::Rc<std::cell::RefCell<(Vec<ViewKey>, usize)>>;

/// Per-statement bind-parameter types, indexed by parameter number minus one.
/// `None` in a slot means the type is not yet known; the extended protocol may
/// seed some slots from the client's declared OID list.
///
#[derive(Debug)]
pub struct ParamState {
    /// `types[i]` is the type of `$(i+1)`; `None` until inferred/declared.
    types: Vec<Option<PgType>>,
    /// Whether `$n` placeholders are permitted at all. A simple-query bind sets
    /// this false, so any `$n` is PG's `42P02` "there is no parameter $n".
    allow: bool,
    /// The highest valid parameter number, when the caller knows it up front
    /// (a SQL function body has exactly `$1..$max` for its declared arguments).
    /// `None` leaves the wire-protocol bound (`MAX_PARAMS`) as the only cap.
    max: Option<usize>,
    /// View expansion in progress, shared with any nested view body.
    views: ViewExpansion,
}

/// Two parameter contexts are equal when they describe the same `$n` types. The
/// view-expansion state is transient bookkeeping for an in-flight bind, not part
/// of a context's identity — and comparing it would borrow a cell the binder may
/// be holding mutably at the time.
impl PartialEq for ParamState {
    fn eq(&self, other: &Self) -> bool {
        self.types == other.types && self.allow == other.allow && self.max == other.max
    }
}

/// Upper bound on a parameter number `$n`. The Bind message carries the values
/// in an `i16`-counted array, so no more than this many can ever be supplied.
const MAX_PARAMS: usize = 65535;

impl ParamState {
    /// Register a `$n` (1-based) occurrence, growing the slot vector as needed
    /// and returning its 0-based index. When placeholders are not allowed (a
    /// simple query), this is PG's `42P02` "there is no parameter $n".
    pub(super) fn reference(&mut self, n1: usize) -> Result<usize, BindError> {
        if !self.allow {
            return Err(BindError::new(
                "42P02",
                format!("there is no parameter ${n1}"),
            ));
        }
        // A caller that declared its parameters (a SQL function body) rejects any
        // `$n` past the last argument here, at the reference site, so the error
        // names the actual `n` — matching PG's "there is no parameter $n".
        if self.max.is_some_and(|max| n1 > max) {
            return Err(BindError::new(
                "42P02",
                format!("there is no parameter ${n1}"),
            ));
        }
        // A parameter number is bounded by the wire protocol (Bind delivers at
        // most 65535 parameter values), so a larger `$n` can never be supplied.
        // Reject it up front rather than resizing the slot vector to an
        // attacker-chosen length — `SELECT $2000000000` would otherwise allocate
        // gigabytes.
        if n1 > MAX_PARAMS {
            return Err(BindError::new(
                "54000",
                format!("there is no parameter ${n1}"),
            ));
        }
        let index = n1 - 1;
        if index >= self.types.len() {
            self.types.resize(index + 1, None);
        }
        Ok(index)
    }

    /// Record that parameter `index` was used in a context of type `ty`. A slot
    /// that already carries a *different* concrete type is PG's `42P18`
    /// "inconsistent types deduced for parameter $n".
    pub(super) fn resolve(&mut self, index: usize, ty: PgType) -> Result<(), BindError> {
        if index >= self.types.len() {
            self.types.resize(index + 1, None);
        }
        match self.types[index] {
            Some(existing) if existing != ty => Err(BindError::new(
                "42P18",
                format!("inconsistent types deduced for parameter ${}", index + 1),
            )),
            _ => {
                self.types[index] = Some(ty);
                Ok(())
            }
        }
    }

    /// The type deduced for `$(index + 1)`, or `None` while the slot is still
    /// open. Reading goes through here so the slot vector itself stays private:
    /// its length is only ever grown by [`ParamState::reference`], under the
    /// `MAX_PARAMS` cap.
    pub(super) fn slot_type(&self, index: usize) -> Option<PgType> {
        self.types.get(index).copied().flatten()
    }
}

/// A parameter context for the extended query protocol: placeholders are
/// allowed, and `declared` seeds the initially-known types (a `None` slot is
/// left to be inferred from context, as PG does for an unspecified OID).
pub fn param_ctx_extended(declared: Vec<Option<PgType>>) -> ParamCtx {
    std::rc::Rc::new(std::cell::RefCell::new(ParamState {
        types: declared,
        allow: true,
        max: None,
        views: Default::default(),
    }))
}

/// A parameter context for a SQL function body: `$1..$declared.len()` are the
/// declared argument types, and any larger `$n` is PG's `42P02` "there is no
/// parameter $n" reported at the reference site with the real `n`.
pub fn param_ctx_capped(declared: Vec<Option<PgType>>) -> ParamCtx {
    let max = Some(declared.len());
    std::rc::Rc::new(std::cell::RefCell::new(ParamState {
        types: declared,
        allow: true,
        max,
        views: Default::default(),
    }))
}

/// A parameter context for the simple query protocol: any `$n` is an error
/// (`42P02`), matching PG, which only accepts parameters via `Parse`/`Bind`.
pub fn param_ctx_none() -> ParamCtx {
    std::rc::Rc::new(std::cell::RefCell::new(ParamState {
        types: Vec::new(),
        allow: false,
        max: None,
        views: Default::default(),
    }))
}

/// A parameter context for a stored view's body. The body references none of the
/// enclosing statement's `$n` (it is standalone SQL text), so parameter state
/// starts empty and disallowed — but the view-expansion state is *shared* with
/// the parent, since that is what lets a cycle be seen at the point it would
/// recurse.
pub(crate) fn param_ctx_view_body(parent: &ParamCtx) -> ParamCtx {
    let views = std::rc::Rc::clone(&parent.borrow().views);
    std::rc::Rc::new(std::cell::RefCell::new(ParamState {
        types: Vec::new(),
        allow: false,
        max: None,
        views,
    }))
}

/// The view-expansion state, as a handle that outlives any borrow of the
/// `ParamState` itself — so a caller never holds a `ParamCtx` borrow across the
/// nested bind it is guarding.
pub(crate) fn view_expansion(ctx: &ParamCtx) -> ViewExpansion {
    std::rc::Rc::clone(&ctx.borrow().views)
}

/// The current inferred/declared parameter types (index = parameter number − 1).
/// The caller reads this after a successful bind to describe the statement's
/// parameters; a `None` slot is a parameter whose type could not be determined.
pub fn param_types(ctx: &ParamCtx) -> Vec<Option<PgType>> {
    ctx.borrow().types.clone()
}

/// Fail with PG's `42P18` "could not determine data type of parameter $n" for
/// the first parameter whose type is still unknown after binding. The extended
/// protocol calls this before describing a statement.
pub fn require_all_resolved(ctx: &ParamCtx) -> Result<(), BindError> {
    let state = ctx.borrow();
    for (i, ty) in state.types.iter().enumerate() {
        if ty.is_none() {
            return Err(BindError::new(
                "42P18",
                format!("could not determine data type of parameter ${}", i + 1),
            ));
        }
    }
    Ok(())
}
