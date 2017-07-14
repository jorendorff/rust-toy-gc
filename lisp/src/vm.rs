//! If you use enough force, you can actually use this GC to implement a toy VM.

use builtins::{self, BuiltinFnPtr};
use cell_gc::{GcLeaf, GcHeapSession};
use cell_gc::collections::VecRef;
use parser;
use std::sync::Arc;

#[derive(Debug, IntoHeap)]
pub struct Pair<'h> {
    pub car: Value<'h>,
    pub cdr: Value<'h>,
}

#[derive(Clone, Debug, PartialEq, IntoHeap)]
pub enum Value<'h> {
    Nil,
    Bool(bool),
    Int(i32),
    Symbol(Arc<String>),
    Lambda(PairRef<'h>),
    Builtin(GcLeaf<BuiltinFnPtr>),
    Cons(PairRef<'h>),
    Vector(VecRef<'h, Value<'h>>),
}

pub use self::Value::*;

impl<'h> Value<'h> {
    fn null(&self) -> bool {
        match *self {
            Nil => true,
            _ => false
        }
    }

    pub fn is_boolean(&self) -> bool {
        match *self {
            Bool(_) => true,
            _ => false,
        }
    }

    /// True unless this value is `#f`. Conditional expressions (`if`, `cond`,
    /// etc.) should use this to check whether a value is a "true value".
    pub fn to_bool(&self) -> bool {
        match *self {
            Bool(false) => false,
            _ => true
        }
    }

    pub fn is_pair(&self) -> bool {
        match *self {
            Cons(_) => true,
            _ => false
        }
    }

    fn as_symbol(self, error_msg: &str) -> Result<Arc<String>, String> {
        match self {
            Symbol(s) => Ok(s),
            _ => Err(error_msg.to_string())
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Environment<'h>(pub Value<'h>);

impl<'h> Environment<'h> {
    pub fn default_env(hs: &mut GcHeapSession<'h>) -> Environment<'h> {
        let mut env = Environment(Nil);

        macro_rules! builtin {
            ( $lisp_ident:expr , $func:expr ) => {
                env.push(hs,
                         Arc::new($lisp_ident.to_string()),
                         Builtin(GcLeaf::new(BuiltinFnPtr($func))));
            }
        }

        builtin!("+", builtins::add);
        builtin!("-", builtins::sub);
        builtin!("*", builtins::mul);
        builtin!("assert", builtins::assert);
        builtin!("car", builtins::car);
        builtin!("cdr", builtins::cdr);
        builtin!("cons", builtins::cons);
        builtin!("eq?", builtins::eq_question);
        builtin!("print", builtins::print);
        builtin!("boolean?", builtins::boolean_question);
        builtin!("pair?", builtins::pair_question);

        const PRELUDE: &'static str = include_str!("prelude.sch");
        let prelude = match parser::parse(hs, PRELUDE) {
            Ok(forms) => forms,
            Err(err) => panic!("unexpected error parsing prelude: {}", err)
        };
        for expr in prelude {
            let result = eval(hs, expr, env);
            match result {
                Ok((_, new_env)) => { env = new_env; }
                Err(err) => panic!("unexpected error evaluating prelude: {}", err)
            }
        }
        env
    }

    pub fn push(&mut self, hs: &mut GcHeapSession<'h>, key: Arc<String>, value: Value<'h>) {
        let pair = Cons(hs.alloc(Pair {
            car: Symbol(key),
            cdr: value,
        }));
        *self = Environment(Cons(hs.alloc(Pair {
            car: pair,
            cdr: self.0.clone(),
        })));
    }

    pub fn lookup(&self, name: Arc<String>) -> Result<PairRef<'h>, String> {
        let mut env = self.0.clone();
        let v = Symbol(name.clone());
        while let Cons(p) = env {
            match p.car() {
                Cons(rib) => {
                    if rib.car() == v {
                        return Ok(rib);
                    }
                }
                _ => {
                    panic!("internal error: bad environment structure");
                }
            }
            env = p.cdr();
        }
        assert!(env.null(), "internal error: bad environment structure (improper list)");
        Err(format!("undefined symbol: {:?}", *name))
    }

    pub fn get(&self, name: Arc<String>) -> Result<Value<'h>, String> {
        Ok(self.lookup(name)?.cdr())
    }

    pub fn set(&self, name: Arc<String>, value: Value<'h>) -> Result<(), String> {
        self.lookup(name)?.set_cdr(value);
        Ok(())
    }
}

#[macro_export]
macro_rules! lisp {
    { ( ) , $_hs:expr } => {
        Nil
    };
    { ( $h:tt $($t:tt)* ) , $hs:expr } => {
        {
            let h = lisp!($h, $hs);
            let t = lisp!(($($t)*), $hs);
            Cons($hs.alloc(Pair { car: h, cdr: t }))
        }
    };
    { $s:tt , $_hs:expr } => {
        {
            let s = stringify!($s);  // lame, but nothing else matches after an `ident` match fails
            if s.starts_with(|c: char| c.is_digit(10)) {
                Int(s.parse().expect("invalid numeric literal in `lisp!`"))
            } else {
                Symbol(Arc::new(s.to_string()))
            }
        }
    };
}

fn parse_pair<'h>(v: Value<'h>, msg: &'static str) -> Result<(Value<'h>, Value<'h>), String> {
    match v {
        Cons(r) => Ok((r.car(), r.cdr())),
        _ => Err(msg.to_string()),
    }
}

fn eval_each<'h, F>(
    hs: &mut GcHeapSession<'h>,
    mut exprs: Value<'h>,
    mut env: Environment<'h>,
    mut f: F,
) -> Result<Environment<'h>, String>
    where F: FnMut(&mut GcHeapSession<'h>, Value<'h>) -> Result<(), String>
{
    while let Cons(pair) = exprs {
        let (val, new_env) = eval(hs, pair.car(), env)?;
        env = new_env;
        f(hs, val)?;
        exprs = pair.cdr();
    }
    match exprs {
        Nil => Ok(env),
        _ => Err("improper list of expressions".to_string())
    }
}

fn map_eval<'h>(
    hs: &mut GcHeapSession<'h>,
    exprs: Value<'h>,
    env: Environment<'h>,
) -> Result<(Vec<Value<'h>>, Environment<'h>), String> {
    let mut v = vec![];
    let env = eval_each(hs, exprs, env, |_hs, val| { v.push(val); Ok(()) })?;
    Ok((v, env))
}

pub fn eval_block_body<'h>(
    hs: &mut GcHeapSession<'h>,
    exprs: Value<'h>,
    env: Environment<'h>,
) -> Result<Value<'h>, String> {
    let mut v = Nil;
    let _ = eval_each(hs, exprs, env, |_hs, val| { v = val; Ok(()) })?;
    Ok(v)
}

fn apply<'h>(
    hs: &mut GcHeapSession<'h>,
    fval: Value<'h>,
    args: Vec<Value<'h>>,
) -> Result<Value<'h>, String> {
    match fval {
        Builtin(f) => (f.0)(hs, args),
        Lambda(pair) => {
            let mut env = Environment(pair.cdr());
            let (mut params, body) = parse_pair(pair.car(), "syntax error in lambda")?;

            let mut i = 0;
            while let Cons(pair) = params {
                if i > args.len() {
                    return Err("apply: not enough arguments".to_string());
                }
                if let Symbol(s) = pair.car() {
                    let pair = Cons(hs.alloc(Pair {
                        car: Symbol(s),
                        cdr: args[i].clone(),
                    }));
                    env = Environment(Cons(hs.alloc(Pair {
                        car: pair,
                        cdr: env.0,
                    })));
                } else {
                    return Err("syntax error in lambda arguments".to_string());
                }
                params = pair.cdr();
                i += 1;
            }
            if i < args.len() {
                return Err("apply: too many arguments".to_string());
            }
            let result = eval_block_body(hs, body, env)?;
            Ok(result)
        }
        _ => Err("apply: not a function".to_string()),
    }
}

/// Evaluate the give expression in the given environment, and return the
/// resulting value and new environment.
pub fn eval<'h>(
    hs: &mut GcHeapSession<'h>,
    expr: Value<'h>,
    env: Environment<'h>,
) -> Result<(Value<'h>, Environment<'h>), String> {
    match expr {
        Symbol(s) => Ok((env.get(s)?, env)),
        Cons(p) => {
            let f = p.car();
            if let Symbol(ref s) = f {
                if &**s == "lambda" {
                    return Ok((
                        Lambda(hs.alloc(Pair {
                            car: p.cdr(),
                            cdr: env.0.clone(),
                        })),
                        env,
                    ));
                } else if &**s == "quote" {
                    let (datum, rest) = parse_pair(p.cdr(), "(quote) with no arguments")?;
                    if !rest.null() {
                        return Err("too many arguments to (quote)".to_string());
                    }
                    return Ok((datum, env));
                } else if &**s == "if" {
                    let (cond, rest) = parse_pair(p.cdr(), "(if) with no arguments")?;
                    let (t_expr, rest) = parse_pair(rest, "missing arguments after (if COND)")?;
                    let (f_expr, rest) =
                        parse_pair(rest, "missing 'else' argument after (if COND X)")?;
                    if !rest.null() {
                        return Err("too many arguments in (if) expression".to_string());
                    }
                    let (cond_result, env) = eval(hs, cond, env)?;
                    let selected_expr = if cond_result.to_bool() { t_expr } else { f_expr };
                    return eval(hs, selected_expr, env);
                } else if &**s == "begin" {
                    let result = eval_block_body(hs, p.cdr(), env.clone())?;
                    return Ok((result, env));
                } else if &**s == "define" {
                    let (name, rest) = parse_pair(p.cdr(), "(define) with no name")?;
                    match name {
                        Symbol(s) => {
                            let (val, rest) = parse_pair(rest, "(define) with no value")?;
                            match rest {
                                Nil => {}
                                _ => return Err("too many items in (define) special form".to_string()),
                            };

                            let (val, mut env) = eval(hs, val, env)?;
                            env.push(hs, s, val);
                            return Ok((Nil, env));
                        }
                        Cons(pair) => {
                            let name = match pair.car() {
                                Symbol(s) => s,
                                _ => return Err("(define) with no name".to_string()),
                            };
                            let code = Cons(hs.alloc(Pair {
                                car: pair.cdr(), // formals
                                cdr: rest,
                            }));
                            let f = Lambda(hs.alloc(Pair {
                                car: code,
                                cdr: env.0.clone(),
                            }));
                            let mut env = env;
                            env.push(hs, name, f);
                            return Ok((Nil, env));
                        }
                        _ => {
                            return Err("(define) with a non-symbol name".to_string());
                        }
                    }
                } else if &**s == "set!" {
                    let (first, rest) = parse_pair(p.cdr(), "(set!) with no name")?;
                    let name = first.as_symbol("(set!) first argument must be a name")?;
                    let (expr, rest) = parse_pair(rest, "(set!) with no value")?;
                    if !rest.null() {
                        return Err("(set!): too many arguments".to_string());
                    }
                    let (val, _) = eval(hs, expr, env.clone())?;
                    env.set(name, val)?;
                    return Ok((Nil, env));
                }
            }
            let (fval, env) = eval(hs, f, env)?;
            let (args, env) = map_eval(hs, p.cdr(), env)?;
            Ok((apply(hs, fval, args)?, env))
        }
        Builtin(_) => Err(format!("builtin function found in source code")),
        Vector(_) => Err(format!("vectors are not expressions")),
        _ => Ok((expr, env)),  // nil and numbers are self-evaluating
    }
}

#[cfg(test)]
include!("tests.rs");
