//! A routine call's variable frame, and the control-flow signals a statement
//! can produce.

use crabgresql_executor::ExecError;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{PgType, Value};

use crate::ast::VarId;

/// What a statement asks its enclosing construct to do next. `Normal` is the
/// overwhelmingly common case; the rest unwind through nested statement lists
/// until something claims them.
pub enum Flow {
    Normal,
    /// `EXIT [label]` — leave the named loop or block, or the innermost loop.
    Exit(Option<String>),
    /// `CONTINUE [label]` — start the named loop's next iteration.
    Continue(Option<String>),
    /// `RETURN expr` — leave the routine with a value.
    Return(Value),
}

/// One variable's slot.
#[derive(Clone)]
struct Slot {
    value: Value,
    /// The declared type, resolved when the declaration was executed. `None`
    /// for a slot nothing has declared yet, which cannot be read.
    ty: Option<PgType>,
    constant: bool,
    not_null: bool,
    /// The name as written, for error text.
    name: String,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            value: Value::Null,
            ty: None,
            constant: false,
            not_null: false,
            name: String::new(),
        }
    }
}

/// A routine call's variables.
///
/// Flat, and sized once at entry: the compiler assigns every declaration in
/// every nested block its own slot, so entering a block never resizes the frame
/// and a slot index is a plain array offset. Re-entering a block (a loop body
/// with declarations) just re-initializes the same slots.
pub struct Frame {
    slots: Vec<Slot>,
    /// The slot holding `FOUND`, which several statements update.
    found: Option<VarId>,
    /// The innermost statement this invocation has begun executing, as
    /// `(line, context label)`.
    ///
    /// PostgreSQL reports exactly one `CONTEXT:` frame per routine invocation,
    /// naming the statement that invocation was on — not one frame per
    /// enclosing statement. Recording the deepest statement entered (and never
    /// restoring it while unwinding) leaves precisely that statement here when
    /// an error escapes, so the interpreter can push one frame at the
    /// invocation boundary.
    current: Option<(u32, &'static str)>,
}

impl Frame {
    pub fn new(nvars: usize) -> Self {
        Self {
            slots: vec![Slot::default(); nvars],
            found: None,
            current: None,
        }
    }

    /// Record that this invocation has begun executing a statement. Called on
    /// the way *in*, so nesting overwrites with the innermost statement.
    pub fn enter_statement(&mut self, line: u32, label: &'static str) {
        self.current = Some((line, label));
    }

    /// The statement to name in this invocation's `CONTEXT:` frame.
    pub fn current_statement(&self) -> Option<(u32, &'static str)> {
        self.current
    }

    /// Set a slot's value and type outright, bypassing the CONSTANT check —
    /// used for arguments, declaration initializers and the `FOR` loop
    /// variable, none of which are assignments.
    pub fn init_slot(&mut self, var: VarId, value: Value, ty: Option<PgType>) {
        if let Some(slot) = self.slots.get_mut(var.0) {
            slot.value = value;
            slot.ty = ty;
            slot.constant = false;
            slot.not_null = false;
        }
        // The first slot initialized as a bool named nothing is FOUND; the
        // interpreter tells us explicitly instead of guessing.
    }

    /// Record a declaration's modifiers after its initializer has run, so the
    /// initializer itself is not rejected as an assignment to a CONSTANT.
    pub fn set_flags(&mut self, var: VarId, constant: bool, not_null: bool, name: &str) {
        if let Some(slot) = self.slots.get_mut(var.0) {
            slot.constant = constant;
            slot.not_null = not_null;
            slot.name = name.to_string();
        }
    }

    /// Which slot holds `FOUND`.
    pub fn track_found(&mut self, var: VarId) {
        self.found = Some(var);
    }

    pub fn set_found(&mut self, found: bool) {
        if let Some(var) = self.found
            && let Some(slot) = self.slots.get_mut(var.0)
        {
            slot.value = Value::Bool(found);
        }
    }

    /// A slot's current value.
    pub fn get(&self, var: VarId) -> Result<Value, ExecError> {
        self.slots
            .get(var.0)
            .map(|s| s.value.clone())
            .ok_or_else(|| internal(format!("PL/pgSQL variable slot {} is out of range", var.0)))
    }

    /// A slot's declared type, once its declaration has run.
    pub fn type_of(&self, var: VarId) -> Option<PgType> {
        self.slots.get(var.0).and_then(|s| s.ty)
    }

    /// Assign to a slot, enforcing CONSTANT and NOT NULL.
    pub fn assign(&mut self, var: VarId, value: Value) -> Result<(), ExecError> {
        let slot = self
            .slots
            .get_mut(var.0)
            .ok_or_else(|| internal(format!("PL/pgSQL variable slot {} is out of range", var.0)))?;
        if slot.constant {
            return Err(ExecError::new(
                sqlstate::SYNTAX_ERROR,
                format!("variable \"{}\" is declared CONSTANT", slot.name),
            ));
        }
        if slot.not_null && matches!(value, Value::Null) {
            return Err(ExecError::new(
                "23502",
                format!(
                    "null value cannot be assigned to variable \"{}\" declared NOT NULL",
                    slot.name
                ),
            ));
        }
        slot.value = value;
        Ok(())
    }
}

fn internal(message: String) -> ExecError {
    ExecError::new(sqlstate::INTERNAL_ERROR, message)
}
