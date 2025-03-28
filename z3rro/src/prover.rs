//! Not a SAT solver, but a prover. There's a difference.
use thiserror::Error;

use std::{
    collections::VecDeque,
    env,
    fmt::Display,
    io::{self, Write},
    path::Path,
    process::{self, Command},
    time::Duration,
};

use tempfile::NamedTempFile;

use z3::{
    ast::{forall_const, Ast, Bool, Dynamic},
    Context, Model, SatResult, Solver,
};

use crate::{
    model::InstrumentedModel,
    smtlib::Smtlib,
    util::{set_solver_timeout, ReasonUnknown},
};

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("Environment variable error: {0}")]
    EnvVarError(#[from] env::VarError),
    #[error("Process execution failed: {0}")]
    ProcessError(#[from] io::Error),
}

#[derive(Debug)]
pub enum SolverType {
    Z3,
    SWINE,
}

/// The result of a prove query.
#[derive(Debug)]
pub enum ProveResult<'ctx> {
    Proof,
    Counterexample(InstrumentedModel<'ctx>),
    Unknown(ReasonUnknown),
}

/// Execute swine on the file located at file_path
fn execute_swine(file_path: &Path) -> Result<SatResult, CommandError> {
    let output = Command::new("swine").arg(file_path).output();

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);

            if stdout.contains("unsat") {
                Ok(SatResult::Unsat)
            } else if stdout.contains("sat") {
                Ok(SatResult::Sat)
            } else {
                Ok(SatResult::Unknown)
            }
        }
        Err(e) => Err(CommandError::ProcessError(e)),
    }
}

/// In order to execute the program, it is necessary to remove lines that
/// contain a forall quantifier or the declaration of the exponential function (exp).
fn remove_lines_for_swine(input: &str) -> String {
    let mut output = String::new();
    let mut tmp_buffer: VecDeque<char> = VecDeque::new();
    let mut input_buffer: VecDeque<char> = input.chars().collect();
    let mut cnt = 0;

    while let Some(c) = input_buffer.pop_front() {
        tmp_buffer.push_back(c);
        match c {
            '(' => {
                cnt += 1;
            }
            ')' => {
                cnt -= 1;
                if cnt == 0 {
                    let tmp: String = tmp_buffer.iter().collect();
                    if !tmp.contains("declare-fun exp") && !tmp.contains("forall") {
                        output.push_str(&tmp);
                    }
                    tmp_buffer.clear();
                }
            }
            _ => {}
        }
    }
    output
}

impl Display for ProveResult<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProveResult::Proof => f.write_str("Proof"),
            ProveResult::Counterexample(_) => f.write_str("Counterexample"),
            ProveResult::Unknown(reason) => {
                f.write_fmt(format_args!("Unknown (reason: {})", reason))
            }
        }
    }
}

/// A prover wraps a SAT solver, but it's used to prove validity of formulas.
/// It's a bit of a more explicit API to distinguish between assumptions for a
/// proof ([`Prover::add_assumption`]) and provables ([`Prover::add_provable`]).
///
/// It keeps track of whether there are any assertions added to the solver. If
/// there are none, then [`Prover::check_proof`] will return
/// [`ProveResult::Proof`] (instead of [`SatResult::Sat`], i.e.
/// [`ProveResult::Counterexample`]). Therefore, you shouldn't add assertions
/// via [`Prover::solver`] to not mess that tracking up.
///
/// In contrast to [`z3::Solver`], the [`Prover`] requires exclusive ownership
/// to do any modifications of the solver.
#[derive(Debug)]
pub struct Prover<'ctx> {
    /// The underlying solver.
    solver: Solver<'ctx>,
    /// Number of times push was called minus number of times pop was called.
    level: usize,
    /// The minimum level where an assertion was added to the solver.
    min_level_with_provables: Option<usize>,
    smt_solver: SolverType
}

impl<'ctx> Prover<'ctx> {
    /// Create a new prover with the given [`Context`].
    pub fn new(ctx: &'ctx Context, solver_type:SolverType) -> Self {
        Prover {
            solver: Solver::new(ctx),
            level: 0,
            min_level_with_provables: None,
            smt_solver: solver_type,
        }
    }

    pub fn set_timeout(&mut self, duration: Duration) {
        set_solver_timeout(&self.solver, duration);
    }

    /// Add an assumption to this prover.
    pub fn add_assumption(&mut self, value: &Bool<'ctx>) {
        self.solver.assert(value);
    }

    /// Add a proof obligation to this prover. It adds the negated formula to
    /// the underlying SAT solver's assertions.
    ///
    /// We call it `provable` to avoid confusion between the Z3 solver's
    /// `assert` methods.
    pub fn add_provable(&mut self, value: &Bool<'ctx>) {
        self.solver.assert(&value.not());
        self.min_level_with_provables.get_or_insert(self.level);
    }

    pub fn check_proof(&mut self) -> ProveResult<'ctx> {
        self.check_proof_assuming(&[])
    }

    /// Do the SAT check, but consider a check with no provables to be a
    /// [`ProveResult::Proof`].
    pub fn check_proof_assuming(
        &mut self,
        assumptions: &[Bool<'ctx>],
    ) -> ProveResult<'ctx> {
        if self.min_level_with_provables.is_none() {
            return ProveResult::Proof;
        }

        let res;

        match self.smt_solver {
            SolverType::SWINE => {
                let mut smtlib = self.get_smtlib();
                smtlib.add_check_sat();
                let smtlib = smtlib.into_string();
                let mut smt_file: NamedTempFile = NamedTempFile::new().unwrap();
                smt_file
                    .write_all(remove_lines_for_swine(&smtlib).as_bytes())
                    .unwrap();
                let file_path = smt_file.path();

                res = execute_swine(file_path).unwrap_or_else(|e| {
                    eprintln!("{}", e);
                    process::exit(1)
                });
                match res {
                    SatResult::Unsat => ProveResult::Proof,
                    SatResult::Unknown => {
                        // TODO: Determine the correct reason for Unknown
                        ProveResult::Unknown(ReasonUnknown::Other("unknown".to_string()))
                    }
                    SatResult::Sat => {
                        // TODO: Get the model from the output of SWINE
                        process::exit(1)
                    }
                }
            }
            SolverType::Z3 => {
                res = if assumptions.is_empty() {
                    self.solver.check()
                } else {
                    self.solver.check_assumptions(assumptions)
                };
                match res {
                    SatResult::Unsat => ProveResult::Proof,
                    SatResult::Unknown => ProveResult::Unknown(self.get_reason_unknown().unwrap()),
                    SatResult::Sat => {
                        let model = self.get_model().unwrap();
                        let model = InstrumentedModel::new(model);
                        ProveResult::Counterexample(model)
                    }
                }
            }
        }
    }

    /// Do the regular SAT check.
    pub fn check_sat(&mut self) -> SatResult {
        self.solver().check()
    }

    /// Retrieve the model from the solver.
    pub fn get_model(&self) -> Option<Model<'ctx>> {
        self.solver.get_model()
    }

    /// Retrieve the UNSAT core. See [`Solver::get_unsat_core()`].
    pub fn get_unsat_core(&self) -> Vec<Bool<'ctx>> {
        self.solver.get_unsat_core()
    }

    /// See [`Solver::get_reason_unknown`].
    pub fn get_reason_unknown(&self) -> Option<ReasonUnknown> {
        self.solver
            .get_reason_unknown()
            .map(|reason| reason.parse().unwrap())
    }

    /// See [`Solver::push`].
    pub fn push(&mut self) {
        self.solver.push();
        self.level += 1;
    }

    /// See [`Solver::pop`].
    pub fn pop(&mut self) {
        self.solver.pop(1);
        self.level = self.level.checked_sub(1).expect("cannot pop level 0");
        if let Some(prev_min_level) = self.min_level_with_provables {
            // if there are no assertions at this level, remove the counter
            if prev_min_level > self.level {
                self.min_level_with_provables.take();
            }
        }
    }

    /// Retrieve the current stack level. Useful for debug assertions.
    pub fn level(&self) -> usize {
        self.level
    }

    /// Return a reference to the underlying solver. Please do not modifiy it!
    pub fn solver(&self) -> &Solver<'ctx> {
        &self.solver
    }

    /// Turns this prover into a regular [`Solver`].
    pub fn into_solver(self) -> Solver<'ctx> {
        self.solver
    }

    /// Create an exists-forall solver. All constants provided in the iterator
    /// will be universally quantified. The rest will be existentially
    /// quantified.
    ///
    /// The result is a [`Prover`] for convenience (such as using the
    /// [`Self::level()`] function), but it should be used as a [`Solver`] via
    /// [`Self::check_sat()`].
    pub fn to_exists_forall(&self, universal: &[Dynamic<'ctx>]) -> Prover<'ctx> {
        // TODO: what about the params?
        let ctx = self.solver.get_context();
        let universal: Vec<&dyn Ast<'ctx>> =
            universal.iter().map(|v| v as &dyn Ast<'ctx>).collect();
        let assertions = self.solver.get_assertions();
        let theorem = forall_const(ctx, &universal, &[], &Bool::and(ctx, &assertions).not());
        let mut res = Prover::new(ctx, SolverType::Z3);
        res.add_assumption(&theorem);
        res
    }

    /// Return the SMT-LIB that represents the solver state.
    pub fn get_smtlib(&self) -> Smtlib {
        Smtlib::from_solver(&self.solver)
    }
}

#[cfg(test)]
mod test {
    use z3::{ast::Bool, Config, Context, SatResult};

    use crate::prover::SolverType;

    use super::{ProveResult, Prover};

    #[test]
    fn test_prover() {
        let ctx = Context::new(&Config::default());
        let mut prover = Prover::new(&ctx, SolverType::Z3);
        assert!(matches!(prover.check_proof(), ProveResult::Proof));
        assert_eq!(prover.check_sat(), SatResult::Sat);

        prover.push();
        prover.add_assumption(&Bool::from_bool(&ctx, true));
        assert!(matches!(prover.check_proof(), ProveResult::Proof));
        assert_eq!(prover.check_sat(), SatResult::Sat);
        prover.pop();

        assert!(matches!(prover.check_proof(), ProveResult::Proof));
        assert_eq!(prover.check_sat(), SatResult::Sat);
    }
}
