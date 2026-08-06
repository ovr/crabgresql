//! The two-phase symbol table catalog references resolve through.
//!
//! `.dat` files name OIDs by symbol, not by number: `typinput => 'boolin'`
//! points at a `pg_proc` row, while that row's `prorettype => 'bool'` points
//! back at `pg_type`. The reference graph is cyclic — `pg_operator.oprnegate`
//! points into `pg_operator`, `pg_opclass` and `pg_opfamily` point at each
//! other — so no ordering of the files makes a single pass work.
//!
//! Hence two phases, and the split is what this type exists to enforce:
//!
//! 1. **Define.** Every catalog registers the symbols it defines
//!    ([`SymbolTable::define_name`], [`SymbolTable::define_signature`]).
//!    Nothing is resolved yet, so a definition may reference anything.
//! 2. **Resolve.** With all definitions in hand, emission resolves references
//!    in any direction ([`SymbolTable::resolve_name`],
//!    [`SymbolTable::resolve_signature`]).
//!
//! Adding a catalog whose references close a cycle needs no new machinery: give
//! it a [`SymbolKind`], define its symbols in phase one, resolve in phase two.
//!
//! Resolution doubles as a census. Every OID actually pointed at is recorded,
//! so a catalog can emit exactly the rows its inbound references justify
//! (see [`SymbolTable::references`]).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};

/// The namespaces catalog references resolve in. Each `reg*` type in
/// PostgreSQL is one of these: `regproc`/`regprocedure` name a [`Self::Proc`],
/// `regtype` names a [`Self::Type`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SymbolKind {
    Type,
    Proc,
}

#[derive(Default)]
pub struct SymbolTable {
    /// `(kind, name)` -> OID. `None` marks a name carried by more than one
    /// entry: upstream's own bare-name references must be unambiguous, so a
    /// duplicate here means the reference cannot be resolved by name either.
    by_name: HashMap<(SymbolKind, String), Option<u32>>,
    /// `(kind, name(argtype,argtype))` -> OID, the spelling a `regprocedure`
    /// or `regoperator` reference uses.
    by_signature: HashMap<(SymbolKind, String), u32>,
    /// Every OID resolution has handed out, per kind.
    referenced: RefCell<HashMap<SymbolKind, BTreeSet<u32>>>,
    /// The kinds whose census [`Self::references`] has already handed out: a
    /// later resolution that would widen one of them is a bug in the emission
    /// order.
    sealed: RefCell<HashSet<SymbolKind>>,
}

impl SymbolTable {
    /// Phase one: `name` defines `oid`. Defining a name twice poisons it — see
    /// [`Self::by_name`].
    pub fn define_name(&mut self, kind: SymbolKind, name: &str, oid: u32) {
        self.by_name
            .entry((kind, name.to_string()))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(oid));
    }

    /// Phase one: the entry named `name` taking `argtypes` (whitespace- or
    /// comma-separated, as either `.dat` spelling writes them) defines `oid`.
    pub fn define_signature(&mut self, kind: SymbolKind, name: &str, argtypes: &str, oid: u32) {
        self.by_signature
            .insert((kind, signature(name, argtypes)), oid);
    }

    /// Phase two: the OID a bare-name reference points at. `-` (the `.dat`
    /// spelling of "none") and an ambiguous name both resolve to nothing.
    pub fn resolve_name(&self, kind: SymbolKind, name: &str) -> Option<u32> {
        let oid = self
            .by_name
            .get(&(kind, name.to_string()))
            .copied()
            .flatten();
        self.record(kind, oid)
    }

    /// Phase two: the OID a `name(argtype,argtype)` reference points at. `0` is
    /// upstream's spelling of "no object" and names nothing.
    pub fn resolve_signature(&self, kind: SymbolKind, reference: &str) -> Option<u32> {
        let key: String = reference.chars().filter(|c| !c.is_whitespace()).collect();
        let oid = self.by_signature.get(&(kind, key)).copied();
        self.record(kind, oid)
    }

    /// Every OID of `kind` that resolution has handed out so far, ascending.
    /// A catalog emitted last can use this to emit exactly the rows the
    /// catalogs before it point at — each row justified by a reference that
    /// would otherwise dangle.
    ///
    /// Reading a kind's census seals that kind: resolving a *new* OID of it
    /// afterwards would mean a catalog referencing it was emitted after the
    /// census that fed the filter, so that panics instead of silently
    /// dangling. Other kinds stay open — the catalog doing the filtering still
    /// resolves its own columns.
    pub fn references(&self, kind: SymbolKind) -> Vec<u32> {
        self.sealed.borrow_mut().insert(kind);
        self.referenced
            .borrow()
            .get(&kind)
            .map(|oids| oids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Note a resolved OID in the census, passing it through.
    fn record(&self, kind: SymbolKind, oid: Option<u32>) -> Option<u32> {
        if let Some(oid) = oid {
            let fresh = self
                .referenced
                .borrow_mut()
                .entry(kind)
                .or_default()
                .insert(oid);
            assert!(
                !fresh || !self.sealed.borrow().contains(&kind),
                "{kind:?} {oid} was first referenced after the census was read; \
                 emit the catalog that filters on the census last"
            );
        }
        oid
    }
}

/// A signature reference's canonical spelling: `name(arg,arg)`, or `name()` for
/// no arguments. `pg_proc.dat` writes the argument types space-separated,
/// `pg_cast.dat` comma-separated; both land here.
fn signature(name: &str, argtypes: &str) -> String {
    let args: Vec<&str> = argtypes
        .split([' ', '\t', '\n', ','])
        .filter(|a| !a.is_empty())
        .collect();
    format!("{name}({})", args.join(","))
}

#[cfg(test)]
mod tests {
    use super::SymbolKind::{Proc, Type};
    use super::*;

    #[test]
    fn resolves_a_reference_cycle() {
        // `bool`'s input function is `boolin`, whose return type is `bool`:
        // neither catalog can be resolved before the other is defined.
        let mut symbols = SymbolTable::default();
        symbols.define_name(Type, "bool", 16);
        symbols.define_name(Proc, "boolin", 1242);

        assert_eq!(symbols.resolve_name(Proc, "boolin"), Some(1242));
        assert_eq!(symbols.resolve_name(Type, "bool"), Some(16));
    }

    #[test]
    fn an_ambiguous_name_resolves_to_nothing() {
        let mut symbols = SymbolTable::default();
        symbols.define_name(Proc, "float8", 316);
        symbols.define_name(Proc, "float8", 312);
        assert_eq!(symbols.resolve_name(Proc, "float8"), None);
        // A name nobody defined — `-`, the catalog's "no function" — likewise.
        assert_eq!(symbols.resolve_name(Proc, "-"), None);
        // Names live per kind, so poisoning one leaves the other alone.
        symbols.define_name(Type, "float8", 701);
        assert_eq!(symbols.resolve_name(Type, "float8"), Some(701));
    }

    #[test]
    fn a_signature_survives_either_argument_spelling() {
        let mut symbols = SymbolTable::default();
        // As `pg_proc.dat` writes an argument list.
        symbols.define_signature(Proc, "int4", "int2", 313);
        symbols.define_signature(Proc, "now", "", 1299);
        // As `pg_cast.dat` writes a reference to one.
        assert_eq!(symbols.resolve_signature(Proc, "int4(int2)"), Some(313));
        assert_eq!(symbols.resolve_signature(Proc, "now()"), Some(1299));
        // `0` is the spelling of a cast that needs no function.
        assert_eq!(symbols.resolve_signature(Proc, "0"), None);
    }

    #[test]
    fn the_census_holds_what_was_resolved() {
        let mut symbols = SymbolTable::default();
        symbols.define_name(Proc, "boolin", 1242);
        symbols.define_name(Proc, "boolout", 1243);
        symbols.define_name(Type, "bool", 16);

        symbols.resolve_name(Proc, "boolout");
        symbols.resolve_name(Proc, "boolout"); // twice: still one entry
        symbols.resolve_name(Type, "bool");
        // `boolin` is defined but nothing points at it, so it is not referenced.
        assert_eq!(symbols.references(Proc), vec![1243]);
        assert_eq!(symbols.references(Type), vec![16]);
    }

    #[test]
    #[should_panic(expected = "after the census was read")]
    fn resolving_a_new_oid_after_the_census_panics() {
        let mut symbols = SymbolTable::default();
        symbols.define_name(Proc, "boolin", 1242);
        symbols.references(Proc);
        symbols.resolve_name(Proc, "boolin");
    }

    #[test]
    fn the_seal_is_per_kind() {
        // `pg_proc` filters on the proc census, then emits rows whose
        // `prorettype` resolves types for the first time — the kind it read
        // must not lock the kinds it still needs.
        let mut symbols = SymbolTable::default();
        symbols.define_name(Proc, "boolin", 1242);
        symbols.define_name(Type, "bool", 16);
        symbols.resolve_name(Proc, "boolin");
        assert_eq!(symbols.references(Proc), vec![1242]);
        assert_eq!(symbols.resolve_name(Proc, "boolin"), Some(1242));
        assert_eq!(symbols.resolve_name(Type, "bool"), Some(16));
    }
}
