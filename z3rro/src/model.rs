//! SMT objects that can be evaluated in a model to return a concrete SMT type.

use std::{
    cell::RefCell,
    fmt::{self, Display},
    str::FromStr,
};

use num::{BigInt, BigRational};
use thiserror::Error;
use z3::{
    ast::{Ast, Bool, Dynamic, Int, Real},
    FuncDecl, FuncInterp, Model,
};

/// A [`z3::Model`] which keeps track of the accessed constants. This is useful
/// to later print those constants which were not accessed by any of the
/// [`SmtEval`] implementations (e.g. stuff generated by Z3 we don't know
/// about). This way, we can print the whole model and pretty-print everything
/// we know, and then print the rest of the assignments in the model as well.
#[derive(Debug)]
pub struct InstrumentedModel<'ctx> {
    model: Model<'ctx>,
    // TODO: turn this into a HashSet of FuncDecls when the type implements Hash
    accessed_decls: RefCell<im_rc::HashSet<String>>,
    accessed_exprs: RefCell<im_rc::HashSet<Dynamic<'ctx>>>,
}

impl<'ctx> InstrumentedModel<'ctx> {
    pub fn new(model: Model<'ctx>) -> Self {
        InstrumentedModel {
            model,
            accessed_decls: Default::default(),
            accessed_exprs: Default::default(),
        }
    }

    /// Execute this function "atomically" on this model, rolling back any
    /// changes to the list of visited decls/exprs if the function fails with an
    /// error.
    pub fn atomically<T>(
        &self,
        f: impl FnOnce() -> Result<T, SmtEvalError>,
    ) -> Result<T, SmtEvalError> {
        let accessed_decls = self.accessed_decls.borrow().clone();
        let accessed_exprs = self.accessed_exprs.borrow().clone();
        let res = f();
        if res.is_err() {
            *self.accessed_decls.borrow_mut() = accessed_decls;
            *self.accessed_exprs.borrow_mut() = accessed_exprs;
        }
        res
    }

    /// Evaluate the given ast node in this model. `model_completion` indicates
    /// whether the node should be assigned a value even if it is not present in
    /// the model.
    pub fn eval<T: Ast<'ctx>>(&self, ast: &T, model_completion: bool) -> Option<T> {
        self.add_children_accessed(Dynamic::from_ast(ast));
        let res = self.model.eval(ast, model_completion)?;
        Some(res)
    }

    /// Get the function interpretation for this `f`.
    pub fn get_func_interp(&self, f: &FuncDecl<'ctx>) -> Option<FuncInterp<'ctx>> {
        self.accessed_decls.borrow_mut().insert(f.name());
        self.model.get_func_interp(f)
    }

    /// Add this ast node and all its children to the accessed set.
    fn add_children_accessed(&self, ast: Dynamic<'ctx>) {
        if ast.is_const() {
            self.accessed_decls.borrow_mut().insert(ast.decl().name());
        } else if ast.is_app() {
            for child in ast.children() {
                // some Z3 expressions might be extremely big because they
                // contain big expressions repeatedly. so the following check is
                // necessary to avoid walking through these expressions for a
                // very long time.
                let prev = self.accessed_exprs.borrow_mut().insert(child.clone());
                if prev.is_some() {
                    continue;
                }
                self.add_children_accessed(child);
            }
        }
    }

    /// Iterate over all function declarations that were not accessed using
    /// `eval` so far.
    pub fn iter_unaccessed(&self) -> impl Iterator<Item = FuncDecl<'ctx>> + '_ {
        self.model
            .iter()
            .filter(|decl| !self.accessed_decls.borrow().contains(&decl.name()))
    }

    /// Reset the internally tracked accessed declarations and expressions.
    pub fn reset_accessed(&mut self) {
        self.accessed_decls.borrow_mut().clear();
        self.accessed_exprs.borrow_mut().clear();
    }

    pub fn into_model(self) -> Model<'ctx> {
        self.model
    }
}

/// The [`Display`] implementation simply defers to the underlying
/// [`z3::Model`]'s implementation.
impl Display for InstrumentedModel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}", &self.model))
    }
}

#[derive(Error, Debug, Clone)]
pub enum SmtEvalError {
    #[error("solver failed to evaluate a value")]
    EvalError,
    #[error("could not parse value from solver")]
    ParseError,
}

/// SMT objects that can be evaluated to a concrete value given a model.
pub trait SmtEval<'ctx> {
    type Value;

    // TODO: pass a model completion option?
    fn eval(&self, model: &InstrumentedModel<'ctx>) -> Result<Self::Value, SmtEvalError>;
}

impl<'ctx> SmtEval<'ctx> for Bool<'ctx> {
    type Value = bool;

    fn eval(&self, model: &InstrumentedModel<'ctx>) -> Result<bool, SmtEvalError> {
        model
            .eval(self, true)
            .ok_or(SmtEvalError::EvalError)?
            .as_bool()
            .ok_or(SmtEvalError::ParseError)
    }
}

impl<'ctx> SmtEval<'ctx> for Int<'ctx> {
    type Value = BigInt;

    fn eval(&self, model: &InstrumentedModel<'ctx>) -> Result<BigInt, SmtEvalError> {
        // TODO: Z3's as_i64 only returns an i64 value. is there something more complete?
        let value = model
            .eval(self, true)
            .ok_or(SmtEvalError::EvalError)?
            .as_i64()
            .ok_or(SmtEvalError::ParseError)?;
        Ok(BigInt::from(value))
    }
}

impl<'ctx> SmtEval<'ctx> for Real<'ctx> {
    type Value = BigRational;

    fn eval(&self, model: &InstrumentedModel<'ctx>) -> Result<Self::Value, SmtEvalError> {
        let res = model
            .eval(self, false) // TODO
            .ok_or(SmtEvalError::EvalError)?;

        // The .as_real() method only returns a pair of i64 values. If the
        // results don't fit in these types, we start some funky string parsing.
        if let Some((num, den)) = res.as_real() {
            Ok(BigRational::new(num.into(), den.into()))
        } else {
            // we parse a string of the form "(/ num.0 denom.0)"
            let division_expr = format!("{:?}", res);
            if !division_expr.starts_with("(/ ") || !division_expr.ends_with(".0)") {
                return Err(SmtEvalError::ParseError);
            }

            let mut parts = division_expr.split_ascii_whitespace();

            let first_part = parts.next().ok_or(SmtEvalError::ParseError)?;
            if first_part != "(/" {
                return Err(SmtEvalError::ParseError);
            }

            let second_part = parts.next().ok_or(SmtEvalError::ParseError)?;
            let second_part = second_part.replace(".0", "");
            let numerator = BigInt::from_str(&second_part).map_err(|_| SmtEvalError::ParseError)?;

            let third_part = parts.next().ok_or(SmtEvalError::ParseError)?;
            let third_part = third_part.replace(".0)", "");
            let denominator =
                BigInt::from_str(&third_part).map_err(|_| SmtEvalError::ParseError)?;

            Ok(BigRational::new(numerator, denominator))
        }
    }
}
