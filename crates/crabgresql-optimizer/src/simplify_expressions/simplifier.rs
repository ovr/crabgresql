//! The algebraic half of [`super::SimplifyExpressions`]: rewrites that need no
//! evaluation, only the shape of the tree.
//!
//! Runs after constant folding, on the way back up, so it sees the constants
//! folding produced. Today it is the boolean identities — the ones that pay for
//! themselves because a dropped conjunct is a per-row test the executor never
//! runs, and because dropping one can leave the parent constant, which the next
//! optimizer pass then folds.

use crabgresql_binder::{BinOp, BoundExpr};
use crabgresql_types::{PgType, Value};

/// Apply the boolean identities to one node (its children are already
/// simplified). Returns whether the node changed.
pub(super) fn simplify(expr: &mut BoundExpr) -> bool {
    let BoundExpr::Binary {
        op: op @ (BinOp::And | BinOp::Or),
        left,
        right,
        ..
    } = expr
    else {
        return false;
    };
    // The operand value that decides the result on its own: `false` for AND,
    // `true` for OR. The other value is the identity, which simply drops out.
    //
    // NULL is deliberately not decisive: `x AND NULL` is NULL when `x` is true
    // and false when `x` is false, so it cannot be simplified without knowing
    // `x`.
    let decisive = *op == BinOp::Or;
    let keep = match (as_bool(left), as_bool(right)) {
        // `TRUE AND x` → `x`; `FALSE OR x` → `x`.
        (Some(b), _) if b != decisive => Keep::Right,
        (_, Some(b)) if b != decisive => Keep::Left,
        // `FALSE AND x` → `FALSE`; `TRUE OR x` → `TRUE`. This discards the other
        // operand, and with it any error that operand would have raised — which
        // is what PostgreSQL's own simplification does, and why
        // `WHERE 1/x = 1 AND false` returns no rows instead of failing.
        (Some(_), _) | (_, Some(_)) => Keep::Neither,
        (None, None) => return false,
    };
    // Moved, not cloned: the surviving operand can be an arbitrarily large
    // subtree, and this rewrite runs on every AND/OR of every statement.
    let decided = BoundExpr::Const {
        value: Value::Bool(decisive),
        ty: PgType::Bool,
    };
    let kept = match keep {
        Keep::Left => std::mem::replace(&mut **left, decided),
        Keep::Right => std::mem::replace(&mut **right, decided),
        Keep::Neither => decided,
    };
    *expr = kept;
    true
}

/// Which operand of an AND/OR survives the rewrite.
enum Keep {
    Left,
    Right,
    /// Neither: the node collapses to the deciding constant.
    Neither,
}

/// The literal boolean this operand *is*, if it is one. A NULL constant answers
/// `None`: it is a constant, but not one that decides an AND or an OR.
fn as_bool(expr: &BoundExpr) -> Option<bool> {
    match expr {
        BoundExpr::Const {
            value: Value::Bool(b),
            ..
        } => Some(*b),
        _ => None,
    }
}
