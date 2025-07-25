use super::Id;
use super::Num;
use super::Pat;
use super::Str;
use crate::eval::Context;
use crate::eval::Error;
use crate::eval::Eval;
use crate::eval::Test;
use crate::eval::Value;

/// A leaf node within a test set expression such as an identifier or literal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Atom {
    /// A variable.
    Id(Id),

    /// A number literal.
    Num(Num),

    /// A string literal.
    Str(Str),

    /// A pattern literal.
    Pat(Pat),
}

impl<T: Test> Eval<T> for Atom {
    fn eval(&self, ctx: &Context<T>) -> Result<Value<T>, Error> {
        Ok(match self {
            Self::Id(id) => id.eval(ctx)?,
            Self::Num(n) => Value::Num(*n),
            Self::Str(s) => Value::Str(s.clone()),
            Self::Pat(pat) => pat.eval(ctx)?,
        })
    }
}
