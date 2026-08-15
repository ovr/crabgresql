//! `PG_OPERATOR_ROWS` codegen — phase one so far.
//!
//! An operator is referenced by signature and never by name, so
//! [`define_symbols`] is what lets `pg_amop.amopopr` resolve. Emission of the
//! rows themselves follows.

use crate::dat::{Entry, get, oid_field, str_field};
use crate::symbols::SymbolKind::Operator;
use crate::symbols::SymbolTable;

/// The signature an operator is referenced by: `oprname(oprleft,oprright)`.
///
/// A prefix operator writes its absent left operand as the type `0`, which
/// travels into the key as written — see [`SymbolKind::Operator`].
pub fn signature(e: &Entry) -> (String, String) {
    let name = get(e, "oprname")
        .unwrap_or_else(|| panic!("pg_operator entry without oprname"))
        .to_string();
    let operands = format!(
        "{},{}",
        str_field(e, "oprleft", "0"),
        str_field(e, "oprright", "0")
    );
    (name, operands)
}

/// Phase one: every operator, under the `name(left,right)` spelling
/// `pg_amop.amopopr`, `pg_aggregate.aggsortop` and this catalog's own
/// `oprcom`/`oprnegate` reference it by.
pub fn define_symbols(entries: &[Entry], symbols: &mut SymbolTable) {
    for e in entries {
        let (name, operands) = signature(e);
        symbols.define_signature(Operator, &name, &operands, oid_field(e, "oid"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dat::parse_dat;

    #[test]
    fn an_operator_is_keyed_by_its_operands() {
        // `=` names ninety-odd operators; only the operand types tell them
        // apart, and that is the spelling every reference uses.
        let ops = parse_dat(
            "[{ oid => '15', oprname => '=', oprleft => 'int4', oprright => 'int8' },\n\
              { oid => '410', oprname => '=', oprleft => 'int8', oprright => 'int8' },\n\
              { oid => '484', oprname => '-', oprkind => 'l', oprleft => '0', \
             oprright => 'int8' }]",
        );
        let mut symbols = SymbolTable::default();
        define_symbols(&ops, &mut symbols);
        assert_eq!(
            symbols.resolve_signature(Operator, "=(int4,int8)"),
            Some(15)
        );
        assert_eq!(
            symbols.resolve_signature(Operator, "=(int8,int8)"),
            Some(410)
        );
        // A prefix operator keys off the `0` its data writes, so a reference
        // to one resolves rather than dangling.
        assert_eq!(symbols.resolve_signature(Operator, "-(0,int8)"), Some(484));
        assert_eq!(symbols.resolve_signature(Operator, "=(int4,int4)"), None);
    }
}
