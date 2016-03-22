use hubris_syntax::ast::HasSpan;
use super::TyCtxt;
use super::constraint::*;
use super::super::session::{HasSession, Session, Reportable};
use core::{Term, Binder, Name};

use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::rc::Rc;

pub struct Choice {
    constraints: BinaryHeap<CategorizedConstraint>,
    constraint_mapping: HashMap<Name, Vec<CategorizedConstraint>>,
    solution_mapping: HashMap<Name, (Term, Justification)>,
    assumption_justification: Justification,
    constraint_justification: Justification,
}

pub struct Solver<'tcx> {
    ty_cx: &'tcx mut TyCtxt,
    constraints: BinaryHeap<CategorizedConstraint>,
    constraint_mapping: HashMap<Name, Vec<CategorizedConstraint>>,
    pub solution_mapping: HashMap<Name, (Term, Justification)>,
    choice_stack: Vec<Choice>,
}

#[derive(Debug)]
pub enum Error {
    Simplification(Justification),
    Justification(Justification),
    TypeCk(Box<super::Error>),
}

impl From<super::Error> for Error {
    fn from(err: super::Error) -> Error {
        Error::TypeCk(Box::new(err))
    }
}

impl Reportable for Error {
    fn report(self, cx: &Session) -> io::Result<()> {
        match self {
            Error::Justification(j) => match j {
                Justification::Asserted(by) => match by {
                    AssertedBy::Application(span, u, t) =>
                        cx.span_error(span,
                            format!("a term with type `{}` can not be applied to an argument with \
                                     type `{}`", u, t)),
                    AssertedBy::ExpectedFound(infer_ty, ty) =>
                        cx.span_error(ty.get_span(),
                            format!("expected type `{}` found `{}`", ty, infer_ty)),
                },
                Justification::Assumption => cx.error("assumption".to_string()),
                j @ Justification::Join(_, _) => cx.error(format!("{}", j)),
            },
            _ => panic!()
        }
    }
}

impl<'tcx> Solver<'tcx> {
    fn empty(ty_cx: &'tcx mut TyCtxt) -> Solver<'tcx> {
        Solver {
            ty_cx: ty_cx,
            constraints: BinaryHeap::new(),
            constraint_mapping: HashMap::new(),
            solution_mapping: HashMap::new(),
            choice_stack: vec![],
        }
    }

    /// Take a typing context and a sequence of constraints, and setup an
    /// instance of the solver.
    pub fn new(ty_cx: &'tcx mut TyCtxt, cs: ConstraintSeq) -> Result<Solver, Error> {
        let mut solver = Solver::empty(ty_cx);
        for c in cs {
            match &c {
                &Constraint::Unification(ref t, ref u, ref j) => {
                    let simple_cs =
                        try!(solver.simplify(t.clone(), u.clone(), j.clone()));
                    for sc in simple_cs {
                        try!(solver.visit(sc));
                    }
                },
                &Constraint::Choice(..) => {
                    try!(solver.visit(c.clone().categorize()))
                }
            }
        }
        Ok(solver)
    }

    pub fn visit(&mut self, c: CategorizedConstraint) -> Result<(), Error> {
        let CategorizedConstraint {
            category,
            constraint,
        } = c;

        match constraint {
            Constraint::Unification(t, u, j) =>
                self.visit_unification(t, u, j, category),
            Constraint::Choice(..) =>
                panic!("choice constraints aren't impl"),
        }
    }

    pub fn solution_for(&self, name: &Name) -> Option<(Term, Justification)> {
        self.solution_mapping.get(name).map(|x| x.clone())
    }

    pub fn visit_unification(&mut self, r: Term, s: Term, j: Justification, category: ConstraintCategory) -> Result<(), Error> {
        debug!("visit_unification: r={} s={}", r, s);

        for (m, sol) in &self.solution_mapping {
            debug!("solution: {}={}", m, sol.0);
        }

        // Find the correct meta-variable to solve for,
        // either.
        let meta = match (r.is_stuck(), s.is_stuck()) {
            (Some(m1), Some(m2)) => {
                if self.solution_for(&m1).is_some() {
                    m1
                } else {
                    m2
                }
            }
            (Some(m), None) | (None, Some(m)) => {
                m
            }
            _ => panic!("one of these should be stuck otherwise the constraint should be gone already I think?"),
        };

        debug!("meta {}", meta);
        // See if we have a solution in the solution map,
        // if we have a solution for ?m we should substitute
        // it in both terms and reconstruct the equality
        // constraint.
        //
        // Finally we need to visit every constraint that
        // results.
        if let Some((t, j_m)) = self.solution_for(&meta) {
            let simp_c = try!(self.simplify(
                r.instantiate_meta(&meta, &t),
                s.instantiate_meta(&meta, &t),
                j.join(j_m)));

            for sc in simp_c {
                try!(self.visit(sc));
            }

            Ok(())
        } else if category == ConstraintCategory::Pattern {
            debug!("r: {} u: {}", r, s);
            // left or right?
            let meta = match r.head().unwrap_or_else(|| s.head().unwrap()) {
                Term::Var { name } => name,
                _ => panic!("mis-idetnfied pattern constraint")
            };

            let locals = r.args().unwrap_or(vec![]);

            debug!("meta {}", meta);
            // There is a case here I'm not sure about
            // what if the meta variable we solve has been
            // also applied to non-local constants?

            // Currently we just filter map, and don't
            // abstract over those.
            let locals: Vec<_> =
                locals.into_iter().filter_map(|l|
                match l {
                    Term::Var { ref name } if name.is_local() => Some(name.clone()),
                    Term::Var { .. } => None,
                    _ => panic!("mis-idetnfied pattern constraint")
                }).collect();

            let solution = Term::abstract_lambda(locals, s);

            assert!(meta.is_meta());

            self.solution_mapping.insert(meta.clone(), (solution, j));

            let cs = match self.constraint_mapping.get(&meta) {
                None => vec![],
                Some(cs) => cs.clone(),
            };

            for c in cs {
                try!(self.visit(c.clone()));
            }

            Ok(())
        } else {
            debug!("category: {:?}", category);

            let cat_constraint = CategorizedConstraint {
                category: category,
                constraint: Constraint::Unification(r, s, j),
            };

            let mut cs = match self.constraint_mapping.remove(&meta) {
                None => vec![],
                Some(cs) => cs,
            };

            cs.push(cat_constraint.clone());

            self.constraint_mapping.insert(meta, cs);
            self.constraints.push(cat_constraint);

            Ok(())
        }
    }

    pub fn simplify(&self, t: Term, u: Term, j: Justification) -> Result<Vec<CategorizedConstraint>, Error> {
        debug!("simplify: t={} u={}", t, u);
        // Case 1: t and u are precisely the same term
        // unification constraints of this form incur
        // no constraints since this is discharge-able here.
        if t == u {
            debug!("equal");
            return Ok(vec![]);
        }

        // Case 2: if t can beta/iota reduce to then
        // we reduce t ==> t' and create a constraint
        // between t' and u (t' = u).
        else if self.ty_cx.is_bi_reducible(&t) {
            debug!("reduce");
            self.simplify(try!(self.ty_cx.eval(&t)), u, j)
        } else if self.ty_cx.is_bi_reducible(&u) {
            debug!("reduce");
            self.simplify(t, try!(self.ty_cx.eval(&u)), j)
        }

        // Case 3: if the head of t and u are constants
        // we should generate constraints between each of their
        // arguments for example l s_1 .. s_n = l t_1 .. t_n
        // creates (s_1 = t_1, j) ... (s_n = t_n, j).
        else if t.head_is_local() && u.head_is_local() && t.head() == u.head() {
            debug!("inside local head t={} u={}", t, u);
            let t_args = t.args().unwrap().into_iter();
            let u_args = u.args().unwrap().into_iter();

            let mut cs = vec![];
            for (s, r) in t_args.zip(u_args) {
                let arg_cs = try!(self.simplify(s, r, j.clone()));
                cs.extend(arg_cs.into_iter())
            }

            Ok(cs)
        }

        else if t.head_is_global() &&
                u.head_is_global() &&
                t.head() == u.head() {
            debug!("head is global");

            let f = t.head().unwrap();

            let t_args_meta_free =
                t.args().map(|args|
                    args.iter().all(|a| !a.is_meta())).unwrap_or(false);

            let u_args_meta_free =
                u.args().map(|args|
                    args.iter().all(|a| !a.is_meta())).unwrap_or(false);

            if f.is_bi_reducible() &&
               t_args_meta_free &&
               u_args_meta_free {
                panic!("var are free")
                    //      self.simplify(t.unfold(f) = u.unfold(f))
                    // } else if !f.reducible() {
                    //     t.args = u.args
                    // } else { panic!() }
            } else if !f.is_bi_reducible() {
                Ok(t.args().unwrap()
                 .into_iter()
                 .zip(u.args().unwrap().into_iter())
                 .map(|(t_i, s_i)| Constraint::Unification(t_i, s_i, j.clone()).categorize())
                 .collect())
            } else {
                panic!("f is reducible but metavars are ")
            }
        }

        // This should be the case dealing with depth, haven't implemented it
        // yet.
        else if false {
            panic!()
        }

        // else if t.is_lambda() && u.is_lambda() {
        //     panic!()
        // }

        else if t.is_forall() && u.is_forall() {
            debug!("forall");
            match (t, u) {
                (Term::Forall { binder: binder1, term: term1, .. },
                 Term::Forall { binder: binder2, term: term2, .. }) => {
                     let ty1 = binder1.ty.clone();
                     let ty2 = binder2.ty;

                     let local = self.ty_cx.local(binder1).to_term();
                     let mut arg_cs = try!(self.simplify(*ty1, *ty2, j.clone()));

                     let t_sub = term1.instantiate(&local);
                     let u_sub = term2.instantiate(&local);

                     let body_cs = try!(self.simplify(t_sub, u_sub, j.clone()));
                     arg_cs.extend(body_cs.into_iter());

                     Ok(arg_cs)
                 }
                 _ => panic!("this should be impossible")
            }
        } else {
            if t.is_stuck().is_some() ||
               u.is_stuck().is_some() {
                Ok(vec![Constraint::Unification(t, u, j).categorize()])
            } else {
                let j = try!(self.eval_justification(j));
                Err(Error::Justification(j))
            }
        }
    }

    /// Will take the justification that was created at constraint generation time, and substitute
    /// all known meta-variable solutions and then simplify it. This is particularly useful in
    /// error reporting where we want to show the simplest term possible.
    fn eval_justification(&self, j: Justification) -> Result<Justification, Error> {
        use super::constraint::Justification::*;
        // panic!("{:?}", j);
        let j = match j {
            Asserted(by) => Asserted(match by {
                AssertedBy::Application(span, t, u) => {
                    let t = try!(self.ty_cx.eval(&try!(replace_metavars(t, &self.solution_mapping))));
                    let u = try!(self.ty_cx.eval(&try!(replace_metavars(u, &self.solution_mapping))));

                    AssertedBy::Application(span, t, u)
                }
                AssertedBy::ExpectedFound(t, u) => {
                    let t = try!(self.ty_cx.eval(&try!(replace_metavars(t, &self.solution_mapping))));
                    let u = try!(self.ty_cx.eval(&try!(replace_metavars(u, &self.solution_mapping))));

                    AssertedBy::ExpectedFound(t, u)
                }
            }),
            Assumption => Assumption,
            Join(j1, j2) => {
                let j1 = try!(self.eval_justification((&*j1).clone()));
                let j2 = try!(self.eval_justification((&*j2).clone()));
                Join(Rc::new(j1), Rc::new(j2))
            }
        };
        Ok(j)
    }

    // The set of constraints should probably be a lazy list.
    fn process(&self, cs: Vec<CategorizedConstraint>, j: Justification) {
        // for c in &self.constraints {
        //     debug!("{:?}", c);
        //     match c.constraint {
        //         Constraint::Choice(..) => panic!("can't process choice constraints"),
        //         Constraint::Unification(..) => {
        //             match c.category {
        //
        //             }
        //         }
        //
        //     }
        // }
        // assert!(self.constraints.len() > 0);
    }

    pub fn solve(mut self) -> Result<HashMap<Name, (Term, Justification)>, Error> {
        while let Some(c) = self.constraints.pop() {
            debug!("{:?}", c);
            match c.constraint {
                Constraint::Choice(..) => panic!("can't process choice constraints"),
                Constraint::Unification(t, u, j) => {
                    for (m, s) in &self.solution_mapping {
                        debug!("{} {}", m, s.0)
                    }
                    match c.category {
                        ConstraintCategory::FlexFlex => {
                            // Need to clean this code up
                            let t_head = match t.head().unwrap() {
                                Term::Var { name , .. } => name,
                                _ => panic!()
                            };

                            let u_head = match t.head().unwrap() {
                                Term::Var { name , .. } => name,
                                _ => panic!()
                            };

                            if self.solution_for(&t_head) == self.solution_for(&u_head) {
                                debug!("t {} u {}", t_head, u_head);
                            } else {
                                panic!("flex-flex solution is not eq")
                            }
                        }
                        cat => panic!("solver can't handle {:?} {} = {} by {:?}", cat, t, u, j)
                    }
                }
            }
        }


        Ok(self.solution_mapping)
    }

    pub fn resolve(&self, just: Justification) -> Result<(), Error> {
        panic!("{:?}", just);
    }
}

pub fn replace_metavars(t: Term, subst_map: &HashMap<Name, (Term, Justification)>) -> Result<Term, Error> {
    use core::Term::*;

    match t {
        App { fun, arg, span } => {
            Ok(App {
                fun: Box::new(try!(replace_metavars(*fun, subst_map))),
                arg: Box::new(try!(replace_metavars(*arg, subst_map))),
                span: span,
            })
        }
        Forall { binder, term, span } => {
            Ok(Forall {
                binder: try!(subst_meta_binder(binder, subst_map)),
                term: Box::new(try!(replace_metavars(*term, subst_map))),
                span: span,
            })
        }
        Lambda { binder, body, span } => {
            Ok(Lambda {
                binder: try!(subst_meta_binder(binder, subst_map)),
                body: Box::new(try!(replace_metavars(*body, subst_map))),
                span: span,
            })
        }
        Var { ref name } if name.is_meta() => {
            match subst_map.get(&name) {
                None => panic!("no solution found for {}", name),
                Some(x) => Ok(x.clone().0)
            }

        }
        v @ Var { .. } => Ok(v),
        Type => Ok(Type),
    }
}

pub fn subst_meta_binder(
        mut b: Binder,
        subst_map: &HashMap<Name, (Term, Justification)>) -> Result<Binder, Error> {
    b.ty = Box::new(try!(replace_metavars(*b.ty, subst_map)));
    Ok(b)
}
