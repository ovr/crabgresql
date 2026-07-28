//! Compiled expressions and their evaluation over a [`Batch`].
//!
//! # Rule V1: a kernel never evaluates a row the row engine would not have
//!
//! The row engine reaches an expression one row at a time, and control flow
//! decides which rows it reaches at all. `project_pipeline` puts `Filter` below
//! `Projection`, so a projection only ever sees surviving rows; `AND` stops at
//! the first `FALSE`; `CASE` evaluates only the winning branch. Batch evaluation
//! has no such control flow — a kernel touches every lane it is handed.
//!
//! That difference is not a performance detail, it is a correctness one. All of
//! these succeed in PostgreSQL and must succeed here:
//!
//! ```sql
//! SELECT 1/x FROM t WHERE x <> 0;                        -- t contains 0
//! SELECT x FROM t WHERE x <> 0 AND 1/x > 0;
//! SELECT CASE WHEN x <> 0 THEN 1/x ELSE 0 END FROM t;
//! ```
//!
//! A kernel that computed `1/x` over the whole batch and masked afterwards would
//! raise `division by zero` on every one. So evaluation carries a [`Selection`]
//! — the rows still live — and every operator that *decides* rows narrows it
//! before evaluating anything that [`VectorExpr::can_raise`].
//!
//! Narrowing is skipped when the operand cannot raise, which is the common case
//! and costs nothing: `a = 1 AND b = 2` evaluates both sides over the full batch.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, BooleanArray, UInt32Array};
use crabgresql_types::{PgType, Value};

use crate::kernels::{self, ArithOp, CmpOp};
use crate::{Batch, BatchError, batch_type_of};

/// The rows of a batch an evaluation still applies to.
///
/// Every array [`eval`] returns has exactly [`Selection::len`] entries, in
/// selection order — so a caller reading position `i` of a result is reading the
/// row named by position `i` of the selection.
#[derive(Clone, Debug)]
pub enum Selection {
    /// Every row of a batch this long. The shape a scan produces, and the one a
    /// filter-free pipeline must not pay an index vector for.
    All(usize),
    /// These row indices, ascending.
    Some(UInt32Array),
}

impl Selection {
    pub fn len(&self) -> usize {
        match self {
            Selection::All(len) => *len,
            Selection::Some(indices) => indices.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Restrict to the rows where `live` is true, treating null as not live.
    ///
    /// `live` is indexed in *this* selection's space, and the result is indexed
    /// in the batch's — so narrowing composes without the caller tracking depth.
    pub fn narrow(&self, live: &BooleanArray) -> Selection {
        let mut kept = Vec::new();
        for position in 0..self.len().min(live.len()) {
            if live.is_valid(position) && live.value(position) {
                kept.push(match self {
                    Selection::All(_) => position as u32,
                    Selection::Some(indices) => indices.value(position),
                });
            }
        }
        Selection::Some(UInt32Array::from(kept))
    }

    /// Gather the selected rows of `array`, which must be indexed in the
    /// batch's space.
    fn gather(&self, array: &ArrayRef) -> Result<ArrayRef, BatchError> {
        match self {
            Selection::All(_) => Ok(Arc::clone(array)),
            Selection::Some(indices) => arrow_select::take::take(array.as_ref(), indices, None)
                .map_err(|error| BatchError::internal(format!("gather selection: {error}"))),
        }
    }
}

/// A compiled expression over a batch.
///
/// Deliberately binder-free: it names column *positions* within a batch rather
/// than relation ordinals, and carries no plan, catalog or transaction. That is
/// what lets a storage engine evaluate one for pushdown without depending on the
/// SQL layer.
#[derive(Clone, Debug)]
pub enum VectorExpr {
    /// A batch column by position, resolved at compile time.
    Column { position: usize, ty: PgType },
    /// A scalar, broadcast across the selection.
    Literal { value: Value, ty: PgType },
    Compare {
        op: CmpOp,
        arg_ty: PgType,
        left: Box<VectorExpr>,
        right: Box<VectorExpr>,
    },
    Arith {
        op: ArithOp,
        ty: PgType,
        left: Box<VectorExpr>,
        right: Box<VectorExpr>,
    },
    /// Three-valued `AND`/`OR`. The right operand is evaluated under a narrowed
    /// selection whenever it can raise; see the module docs.
    Logic {
        op: LogicOp,
        left: Box<VectorExpr>,
        right: Box<VectorExpr>,
    },
    Not(Box<VectorExpr>),
    /// A widening integer cast, admitted because it cannot fail — see
    /// [`kernels::widens`]. Any other cast is refused by the compiler rather
    /// than approximated.
    Widen {
        input: Box<VectorExpr>,
        from: PgType,
        to: PgType,
    },
    IsNull {
        input: Box<VectorExpr>,
        negated: bool,
    },
    /// `CASE WHEN … THEN … ELSE … END`. Each branch is evaluated only over the
    /// rows that reached it, which is what makes the guarded-division idiom safe.
    Case {
        whens: Vec<(VectorExpr, VectorExpr)>,
        otherwise: Option<Box<VectorExpr>>,
        ty: PgType,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicOp {
    And,
    Or,
}

impl VectorExpr {
    /// The PostgreSQL type this expression produces.
    pub fn result_type(&self) -> PgType {
        match self {
            VectorExpr::Column { ty, .. } | VectorExpr::Literal { ty, .. } => *ty,
            VectorExpr::Compare { .. }
            | VectorExpr::Logic { .. }
            | VectorExpr::Not(_)
            | VectorExpr::IsNull { .. } => PgType::Bool,
            VectorExpr::Arith { ty, .. } => *ty,
            VectorExpr::Widen { to, .. } => *to,
            VectorExpr::Case { ty, .. } => *ty,
        }
    }

    /// Whether evaluating this can raise an error.
    ///
    /// Drives Rule V1: an operand that cannot raise is evaluated over the whole
    /// selection, and only a faulting one pays for narrowing. It also lets the
    /// gate admit a plan under `LIMIT` with no blocking node — the row engine
    /// would stop pulling early there, and a batch cannot, so the plan is only
    /// safe if nothing in it could have faulted on the rows never reached.
    ///
    /// Exhaustive with no `_` arm: a new variant must state its answer.
    pub fn can_raise(&self) -> bool {
        match self {
            VectorExpr::Column { .. } | VectorExpr::Literal { .. } => false,
            VectorExpr::Compare { left, right, .. } => left.can_raise() || right.can_raise(),
            // The operator itself can fault, regardless of its operands.
            VectorExpr::Arith { op, left, right, .. } => {
                op.can_raise() || left.can_raise() || right.can_raise()
            }
            VectorExpr::Logic { left, right, .. } => left.can_raise() || right.can_raise(),
            VectorExpr::Not(input) => input.can_raise(),
            // The widening itself is total; only its operand can fault.
            VectorExpr::Widen { input, .. } => input.can_raise(),
            // `IS NULL` inspects validity and never evaluates a value that could
            // fault... but its *input* still has to be computed.
            VectorExpr::IsNull { input, .. } => input.can_raise(),
            VectorExpr::Case {
                whens, otherwise, ..
            } => {
                whens
                    .iter()
                    .any(|(when, then)| when.can_raise() || then.can_raise())
                    || otherwise.as_ref().is_some_and(|e| e.can_raise())
            }
        }
    }
}

/// Evaluate `expr` over every row of `batch`.
pub fn eval_batch(expr: &VectorExpr, batch: &Batch) -> Result<ArrayRef, BatchError> {
    eval(expr, batch, &Selection::All(batch.len()))
}

/// Evaluate `expr` over the rows `selection` names.
///
/// The returned array has `selection.len()` entries, in selection order.
pub fn eval(
    expr: &VectorExpr,
    batch: &Batch,
    selection: &Selection,
) -> Result<ArrayRef, BatchError> {
    match expr {
        VectorExpr::Column { position, ty } => {
            let column = batch.column(*position).ok_or_else(|| {
                BatchError::internal(format!("expression names missing batch column {position}"))
            })?;
            let field = batch.schema().field(*position).ok_or_else(|| {
                BatchError::internal(format!("no schema field for batch column {position}"))
            })?;
            if field.ty != *ty {
                return Err(BatchError::internal(format!(
                    "expression reads column {position} as {} but it holds {}",
                    ty.name(),
                    field.ty.name()
                )));
            }
            selection.gather(column)
        }
        VectorExpr::Literal { value, ty } => kernels::broadcast(value, *ty, selection.len()),
        VectorExpr::Compare {
            op,
            arg_ty,
            left,
            right,
        } => {
            let l = eval(left, batch, selection)?;
            let r = eval(right, batch, selection)?;
            Ok(Arc::new(kernels::compare(*op, *arg_ty, &l, &r)?))
        }
        VectorExpr::Arith {
            op,
            ty,
            left,
            right,
        } => {
            let l = eval(left, batch, selection)?;
            let r = eval(right, batch, selection)?;
            kernels::arith(*op, *ty, &l, &r)
        }
        VectorExpr::Logic { op, left, right } => eval_logic(*op, left, right, batch, selection),
        VectorExpr::Not(input) => {
            let value = eval(input, batch, selection)?;
            let value = as_bool(&value)?;
            arrow_arith::boolean::not(value)
                .map(|b| Arc::new(b) as ArrayRef)
                .map_err(|error| BatchError::internal(format!("negate batch: {error}")))
        }
        VectorExpr::Widen { input, from, to } => {
            let value = eval(input, batch, selection)?;
            kernels::widen(*from, *to, &value)
        }
        VectorExpr::IsNull { input, negated } => {
            let value = eval(input, batch, selection)?;
            let nulls = arrow_array::BooleanArray::from(
                (0..value.len()).map(|i| value.is_null(i) != *negated).collect::<Vec<_>>(),
            );
            Ok(Arc::new(nulls))
        }
        VectorExpr::Case {
            whens,
            otherwise,
            ty,
        } => eval_case(whens, otherwise.as_deref(), *ty, batch, selection),
    }
}

/// Kleene `AND`/`OR`, narrowing before a right operand that can raise.
///
/// `NULL AND FALSE` is `FALSE` and `NULL OR TRUE` is `TRUE`, which arrow's plain
/// `and`/`or` get wrong (they intersect validity) and its `*_kleene` variants get
/// right. The distinction is observable under `NOT`, not just in a projected
/// boolean: `NOT (NULL AND FALSE)` is `TRUE` and selects a row.
fn eval_logic(
    op: LogicOp,
    left: &VectorExpr,
    right: &VectorExpr,
    batch: &Batch,
    selection: &Selection,
) -> Result<ArrayRef, BatchError> {
    let left_value = eval(left, batch, selection)?;
    let left_bool = as_bool(&left_value)?.clone();

    let right_bool = if right.can_raise() {
        // Only the rows the left operand left undecided reach the right one:
        // for `AND` that is TRUE-or-unknown, for `OR` FALSE-or-unknown. This is
        // what keeps `x <> 0 AND 1/x > 0` from dividing by zero.
        let reaching = reaching_rows(op, &left_bool);
        let narrowed = selection.narrow(&reaching);
        let evaluated = eval(right, batch, &narrowed)?;
        let evaluated = as_bool(&evaluated)?;
        // Rows that did not reach the right operand come back null — "unknown"
        // — which is exactly what Kleene logic needs to leave the left operand's
        // verdict standing.
        scatter_bool(evaluated, &reaching, selection.len())?
    } else {
        as_bool(&eval(right, batch, selection)?)?.clone()
    };

    let combine = match op {
        LogicOp::And => arrow_arith::boolean::and_kleene,
        LogicOp::Or => arrow_arith::boolean::or_kleene,
    };
    combine(&left_bool, &right_bool)
        .map(|b| Arc::new(b) as ArrayRef)
        .map_err(|error| BatchError::internal(format!("combine batch booleans: {error}")))
}

/// The rows whose verdict the left operand did not settle.
fn reaching_rows(op: LogicOp, left: &BooleanArray) -> BooleanArray {
    (0..left.len())
        .map(|i| {
            if left.is_null(i) {
                // Unknown: the right operand can still decide it.
                true
            } else {
                match op {
                    LogicOp::And => left.value(i),
                    LogicOp::Or => !left.value(i),
                }
            }
        })
        .collect()
}

/// `CASE`, evaluating each branch only over the rows that reached it.
fn eval_case(
    whens: &[(VectorExpr, VectorExpr)],
    otherwise: Option<&VectorExpr>,
    ty: PgType,
    batch: &Batch,
    selection: &Selection,
) -> Result<ArrayRef, BatchError> {
    let len = selection.len();
    let data_type = batch_type_of(ty).ok_or_else(|| {
        BatchError::internal(format!("{} has no batch representation", ty.name()))
    })?;

    // Rows still looking for a branch. A row leaves this set as soon as one
    // WHEN matches, so no later branch — and no ELSE — is ever evaluated for it.
    let mut unmatched: Vec<bool> = vec![true; len];
    let mut result = arrow_array::new_null_array(&data_type, len);

    for (when, then) in whens {
        if unmatched.iter().all(|open| !open) {
            break;
        }
        let open = BooleanArray::from(unmatched.clone());
        let open_selection = selection.narrow(&open);
        let condition = eval(when, batch, &open_selection)?;
        let condition = as_bool(&condition)?;
        // Lift the branch's verdict back into this CASE's row space, where a row
        // that never reached the WHEN reads as unknown and so does not match.
        let taken = scatter_bool(condition, &open, len)?;

        let branch_selection = selection.narrow(&taken);
        if branch_selection.is_empty() {
            continue;
        }
        let values = eval(then, batch, &branch_selection)?;
        result = merge(&result, &values, &taken)?;

        for (row, open) in unmatched.iter_mut().enumerate() {
            if taken.is_valid(row) && taken.value(row) {
                *open = false;
            }
        }
    }

    if let Some(otherwise) = otherwise {
        let open = BooleanArray::from(unmatched);
        let open_selection = selection.narrow(&open);
        if !open_selection.is_empty() {
            let values = eval(otherwise, batch, &open_selection)?;
            result = merge(&result, &values, &open)?;
        }
    }
    Ok(result)
}

/// Place `values` — one per true entry of `mask`, in order — into a full-length
/// array, leaving every other position as it was in `base`.
fn merge(base: &ArrayRef, values: &ArrayRef, mask: &BooleanArray) -> Result<ArrayRef, BatchError> {
    let scattered = scatter(values, mask, base.len())?;
    // `zip` keeps `scattered` where the mask is true and `base` elsewhere, which
    // is what makes each branch overwrite only its own rows.
    arrow_select::zip::zip(mask, &scattered, base)
        .map_err(|error| BatchError::internal(format!("merge case branch: {error}")))
}

/// Spread `values` across `len` positions, one per true entry of `mask`, in
/// order. Positions the mask did not select become null.
///
/// Implemented as a `take` with a null-carrying index array, so the null
/// placement is arrow's job rather than a hand-written loop per type.
fn scatter(values: &ArrayRef, mask: &BooleanArray, len: usize) -> Result<ArrayRef, BatchError> {
    let mut indices: Vec<Option<u32>> = Vec::with_capacity(len);
    let mut next = 0u32;
    for position in 0..len {
        if mask.is_valid(position) && mask.value(position) {
            indices.push(Some(next));
            next += 1;
        } else {
            indices.push(None);
        }
    }
    if next as usize != values.len() {
        return Err(BatchError::internal(format!(
            "scatter has {} slots but {} values",
            next,
            values.len()
        )));
    }
    arrow_select::take::take(values.as_ref(), &UInt32Array::from(indices), None)
        .map_err(|error| BatchError::internal(format!("scatter values: {error}")))
}

fn scatter_bool(
    values: &BooleanArray,
    mask: &BooleanArray,
    len: usize,
) -> Result<BooleanArray, BatchError> {
    let array: ArrayRef = Arc::new(values.clone());
    let scattered = scatter(&array, mask, len)?;
    Ok(as_bool(&scattered)?.clone())
}

fn as_bool(array: &ArrayRef) -> Result<&BooleanArray, BatchError> {
    array.as_boolean_opt().ok_or_else(|| {
        BatchError::internal(format!(
            "expected a boolean batch column, found {}",
            array.data_type()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BatchField, BatchSchema};
    use arrow_array::Int32Array;

    /// A one-column `int4` batch, which is enough to exercise every rule here.
    fn xs(values: Vec<Option<i32>>) -> Batch {
        let len = values.len();
        let column: ArrayRef = Arc::new(Int32Array::from(values));
        let field = BatchField::new(Some("x".into()), PgType::Int4, -1, true).expect("encodable");
        Batch::new(BatchSchema::new(vec![field]), vec![column], len).expect("valid batch")
    }

    fn col() -> VectorExpr {
        VectorExpr::Column {
            position: 0,
            ty: PgType::Int4,
        }
    }

    fn lit(value: i32) -> VectorExpr {
        VectorExpr::Literal {
            value: Value::Int4(value),
            ty: PgType::Int4,
        }
    }

    fn cmp(op: CmpOp, left: VectorExpr, right: VectorExpr) -> VectorExpr {
        VectorExpr::Compare {
            op,
            arg_ty: PgType::Int4,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn div(left: VectorExpr, right: VectorExpr) -> VectorExpr {
        VectorExpr::Arith {
            op: ArithOp::Div,
            ty: PgType::Int4,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn ints(array: &ArrayRef) -> Vec<Option<i32>> {
        let values = array
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("int32 result");
        (0..values.len())
            .map(|i| (!values.is_null(i)).then(|| values.value(i)))
            .collect()
    }

    fn bools(array: &ArrayRef) -> Vec<Option<bool>> {
        let values = as_bool(array).expect("boolean result");
        (0..values.len())
            .map(|i| (!values.is_null(i)).then(|| values.value(i)))
            .collect()
    }

    /// `SELECT 1/x FROM t WHERE x <> 0` — PostgreSQL returns a row and raises
    /// nothing. Evaluating the projection under the filter's selection is what
    /// reproduces that.
    #[test]
    fn a_projection_never_evaluates_a_row_the_filter_removed() {
        let batch = xs(vec![Some(1), Some(0), Some(2)]);
        let predicate = cmp(CmpOp::NotEq, col(), lit(0));
        let mask = eval_batch(&predicate, &batch).expect("filter");
        let live = Selection::All(batch.len()).narrow(as_bool(&mask).expect("bool"));

        let projection = div(lit(10), col());
        let values = eval(&projection, &batch, &live).expect("no division by zero");
        assert_eq!(ints(&values), vec![Some(10), Some(5)]);
    }

    /// `WHERE x <> 0 AND 1/x > 0`, which PostgreSQL answers without error.
    #[test]
    fn and_does_not_evaluate_its_right_operand_where_the_left_is_false() {
        let batch = xs(vec![Some(1), Some(0), Some(2)]);
        let guarded = VectorExpr::Logic {
            op: LogicOp::And,
            left: Box::new(cmp(CmpOp::NotEq, col(), lit(0))),
            right: Box::new(cmp(CmpOp::Gt, div(lit(10), col()), lit(0))),
        };
        let mask = eval_batch(&guarded, &batch).expect("no division by zero");
        assert_eq!(bools(&mask), vec![Some(true), Some(false), Some(true)]);
    }

    /// The `OR` dual: a TRUE on the left keeps the right operand away.
    #[test]
    fn or_does_not_evaluate_its_right_operand_where_the_left_is_true() {
        let batch = xs(vec![Some(0), Some(5)]);
        let guarded = VectorExpr::Logic {
            op: LogicOp::Or,
            left: Box::new(cmp(CmpOp::Eq, col(), lit(0))),
            right: Box::new(cmp(CmpOp::Gt, div(lit(10), col()), lit(1))),
        };
        let mask = eval_batch(&guarded, &batch).expect("no division by zero");
        assert_eq!(bools(&mask), vec![Some(true), Some(true)]);
    }

    /// `CASE WHEN x <> 0 THEN 1/x ELSE 0 END`. The SQL standard *guarantees*
    /// this one, so a failure here is a hard bug rather than a divergence.
    #[test]
    fn case_evaluates_only_the_branch_a_row_reached() {
        let batch = xs(vec![Some(1), Some(0), Some(5)]);
        let expression = VectorExpr::Case {
            whens: vec![(cmp(CmpOp::NotEq, col(), lit(0)), div(lit(10), col()))],
            otherwise: Some(Box::new(lit(0))),
            ty: PgType::Int4,
        };
        let values = eval_batch(&expression, &batch).expect("no division by zero");
        assert_eq!(ints(&values), vec![Some(10), Some(0), Some(2)]);
    }

    /// Kleene, not validity intersection: `NULL AND FALSE` is FALSE, so its
    /// negation is TRUE and selects a row.
    #[test]
    fn three_valued_logic_matches_sql_not_arrows_default() {
        let batch = xs(vec![Some(1)]);
        let unknown = VectorExpr::Compare {
            op: CmpOp::Eq,
            arg_ty: PgType::Int4,
            left: Box::new(col()),
            right: Box::new(VectorExpr::Literal {
                value: Value::Null,
                ty: PgType::Int4,
            }),
        };
        let expression = VectorExpr::Not(Box::new(VectorExpr::Logic {
            op: LogicOp::And,
            left: Box::new(unknown.clone()),
            right: Box::new(cmp(CmpOp::Eq, col(), lit(999))),
        }));
        assert_eq!(bools(&eval_batch(&expression, &batch).expect("eval")), vec![Some(true)]);

        // And the dual stays unknown: NULL AND TRUE is unknown, so NOT is too.
        let expression = VectorExpr::Not(Box::new(VectorExpr::Logic {
            op: LogicOp::And,
            left: Box::new(unknown),
            right: Box::new(cmp(CmpOp::Eq, col(), lit(1))),
        }));
        assert_eq!(bools(&eval_batch(&expression, &batch).expect("eval")), vec![None]);
    }

    #[test]
    fn an_overflow_on_a_live_row_still_raises() {
        // The narrowing must not become a way to lose real errors: row 0
        // survives the filter and overflows, so the batch must fail.
        let batch = xs(vec![Some(i32::MAX), Some(0)]);
        let expression = VectorExpr::Arith {
            op: ArithOp::Add,
            ty: PgType::Int4,
            left: Box::new(col()),
            right: Box::new(lit(1)),
        };
        let error = eval_batch(&expression, &batch).expect_err("overflow");
        assert_eq!(error.message, "integer out of range");
    }

    #[test]
    fn is_null_reports_validity_not_a_value() {
        let batch = xs(vec![Some(1), None]);
        let expression = VectorExpr::IsNull {
            input: Box::new(col()),
            negated: false,
        };
        assert_eq!(
            bools(&eval_batch(&expression, &batch).expect("eval")),
            vec![Some(false), Some(true)]
        );
        let expression = VectorExpr::IsNull {
            input: Box::new(col()),
            negated: true,
        };
        assert_eq!(
            bools(&eval_batch(&expression, &batch).expect("eval")),
            vec![Some(true), Some(false)]
        );
    }

    #[test]
    fn can_raise_is_true_exactly_when_an_operator_can_fault() {
        assert!(!col().can_raise());
        assert!(!lit(1).can_raise());
        assert!(!cmp(CmpOp::Eq, col(), lit(1)).can_raise());
        assert!(div(lit(1), col()).can_raise());
        assert!(cmp(CmpOp::Gt, div(lit(1), col()), lit(0)).can_raise());
        assert!(
            VectorExpr::Case {
                whens: vec![(cmp(CmpOp::Eq, col(), lit(0)), div(lit(1), col()))],
                otherwise: None,
                ty: PgType::Int4,
            }
            .can_raise()
        );
    }

    #[test]
    fn narrowing_composes_into_the_batchs_index_space() {
        let outer = Selection::All(5);
        let first = outer.narrow(&BooleanArray::from(vec![true, false, true, true, false]));
        assert_eq!(first.len(), 3);
        // `first` names batch rows 0, 2, 3; narrowing again by its second entry
        // must yield batch row 2, not row 1.
        let second = first.narrow(&BooleanArray::from(vec![false, true, false]));
        let Selection::Some(indices) = second else {
            panic!("expected a narrowed selection");
        };
        assert_eq!(indices.values(), &[2]);
    }

    #[test]
    fn a_column_read_at_the_wrong_type_is_refused() {
        let batch = xs(vec![Some(1)]);
        let expression = VectorExpr::Column {
            position: 0,
            ty: PgType::Int8,
        };
        let error = eval_batch(&expression, &batch).expect_err("type mismatch");
        assert_eq!(
            error.message,
            "expression reads column 0 as bigint but it holds integer"
        );
    }
}
