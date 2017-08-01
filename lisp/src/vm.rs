//! Interpreter for compiled code.

use cell_gc::GcHeapSession;
use cell_gc::collections::VecRef;
use compile::Expr;
use env::{Environment, EnvironmentRef};
use errors::Result;
use value::{Pair, Value};
use value::Value::*;

/// A potentially partially evaluated value.
pub enum Trampoline<'h> {
    /// A completely evaluated value.
    Value(Value<'h>),
    /// The continuation of a partial evaluation in tail position. The stack
    /// should be unwound before resumption of its evaluation.
    TailCall {
        func: Value<'h>,
        args: Vec<Value<'h>>,
    },
}

impl<'h> Trampoline<'h> {
    /// Complete the evaluation of this value. Avoids recursion to implement
    /// proper tail calls and keep from blowing the stack.
    pub fn eval(mut self, hs: &mut GcHeapSession<'h>) -> Result<Value<'h>> {
        loop {
            match self {
                Trampoline::Value(v) => {
                    return Ok(v);
                }
                Trampoline::TailCall { func, args } => {
                    self = apply(hs, func, args)?;
                }
            }
        }
    }
}

pub fn apply<'h>(
    hs: &mut GcHeapSession<'h>,
    fval: Value<'h>,
    mut args: Vec<Value<'h>>,
) -> Result<Trampoline<'h>> {
    match fval {
        Builtin(f) => (f.0)(hs, args),
        Lambda(pair) => {
            let code = match pair.car() {
                Code(code) => code,
                _ => panic!("internal error: bad lambda"),
            };
            let parent = Some(match pair.cdr() {
                Environment(pe) => pe,
                _ => panic!("internal error: bad lambda"),
            });
            let senv = code.senv();
            let names = senv.names();
            let n_names = names.len();
            let has_rest = code.rest();

            let n_required_params = n_names - has_rest as usize;
            if args.len() < n_required_params {
                return Err("apply: not enough arguments".into());
            }
            if has_rest {
                let mut rest_list = Nil;
                for v in args.drain(n_required_params..).rev() {
                    rest_list = Cons(hs.alloc(Pair {
                        car: v,
                        cdr: rest_list,
                    }));
                }
                args.push(rest_list);
            } else if args.len() > n_required_params {
                return Err("apply: too many arguments".into());
            }

            let values = hs.alloc(args);
            let env = Environment::new(hs, parent, senv, values);
            eval_compiled_to_tail_call(hs, &env, code.body())
        }
        _ => Err("apply: not a function".into()),
    }
}

/// Evaluate `expr` until we reach a tail call, at which point it is packaged up
/// as a `Trampoline::TailCall` and returned so we can unwind the stack before
/// continuing evaluation.
pub fn eval_compiled_to_tail_call<'h>(
    hs: &mut GcHeapSession<'h>,
    env: &EnvironmentRef<'h>,
    expr: Expr<'h>,
) -> Result<Trampoline<'h>> {
    match expr {
        Expr::Con(k) => Ok(Trampoline::Value(k)),
        Expr::Var(ref s) => Ok(Trampoline::Value(env.dynamic_get(s)?)),
        Expr::FastVar { up_count, index } =>
            Ok(Trampoline::Value(env.get(up_count as usize, index as usize))),
        Expr::Fun(code) => Ok(Trampoline::Value(Lambda(hs.alloc(Pair {
            car: Value::Code(code),
            cdr: Value::Environment(env.clone()),
        })))),
        Expr::App(subexprs) => {
            let func = eval_compiled(hs, env, subexprs.get(0))?;
            let args: Vec<Value<'h>> = (1..subexprs.len())
                .map(|i| eval_compiled(hs, env, subexprs.get(i)))
                .collect::<Result<Vec<Value<'h>>>>()?;
            Ok(Trampoline::TailCall { func, args })
        }
        Expr::Seq(exprs) => {
            let len = exprs.len();
            if len == 0 {
                Ok(Trampoline::Value(Nil))
            } else {
                for i in 0..(len - 1) {
                    eval_compiled(hs, env, exprs.get(i))?;
                }
                eval_compiled_to_tail_call(hs, env, exprs.get(len - 1))
            }
        }
        Expr::If(if_parts) => {
            let cond_value = eval_compiled(hs, env, if_parts.cond())?;
            let selected_expr = if cond_value.to_bool() {
                if_parts.t_expr()
            } else {
                if_parts.f_expr()
            };
            eval_compiled_to_tail_call(hs, env, selected_expr)
        }
        Expr::Letrec(letrec) => {
            let senv = letrec.senv();
            debug_assert_eq!(senv.names().len(), letrec.exprs().len());
            let values: VecRef<'h, Value<'h>> = hs.alloc(
                (0..senv.names().len())
                    .map(|_| Value::Nil)
                    .collect::<Vec<Value<'h>>>(),
            );
            let letrec_env = Environment::new(hs, Some(env.clone()), senv, values.clone());
            let exprs = letrec.exprs();
            for i in 0..exprs.len() {
                let val = eval_compiled(hs, &letrec_env, exprs.get(i))?;
                values.set(i, val);
            }
            eval_compiled_to_tail_call(hs, &letrec_env, letrec.body())
        }
        Expr::Def(def) => {
            let val = eval_compiled(hs, env, def.value())?;
            env.push(def.name().unwrap(), val);
            Ok(Trampoline::Value(Value::Unspecified))
        }
        Expr::Set(def) => {
            let val = eval_compiled(hs, env, def.value())?;
            env.dynamic_set(&def.name(), val)?;
            Ok(Trampoline::Value(Value::Unspecified))
        }
    }
}

pub fn eval_compiled<'h>(
    hs: &mut GcHeapSession<'h>,
    env: &EnvironmentRef<'h>,
    expr: Expr<'h>,
) -> Result<Value<'h>> {
    eval_compiled_to_tail_call(hs, env, expr)?.eval(hs)
}

#[cfg(test)]
include!("tests.rs");
