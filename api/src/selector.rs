use std::collections::BTreeSet;
use std::fmt;

use strum::Display;

#[derive(Clone, Debug)]
pub struct Expr(String);

impl Expr {
    #[inline]
    pub fn new(key: impl ToString) -> Self {
        Self(key.to_string())
    }

    #[inline]
    pub fn eq(self, value: impl ToString) -> Expression {
        Expression::Eq(self.0, value.to_string())
    }

    #[inline]
    pub fn neq(self, value: impl ToString) -> Expression {
        Expression::Neq(self.0, value.to_string())
    }

    #[inline]
    pub fn in_(self, values: impl IntoIterator<Item = impl ToString>) -> Expression {
        Expression::In(self.0, values.into_iter().map(|v| v.to_string()).collect())
    }

    #[inline]
    pub fn not_in(self, values: impl IntoIterator<Item = impl ToString>) -> Expression {
        Expression::NotIn(self.0, values.into_iter().map(|v| v.to_string()).collect())
    }

    #[inline]
    pub fn exists(self) -> Expression {
        Expression::Exists(self.0)
    }

    #[inline]
    pub fn not_exists(self) -> Expression {
        Expression::NotExists(self.0)
    }
}

#[derive(Clone, Debug)]
pub enum Expression {
    Eq(String, String),
    Neq(String, String),
    In(String, BTreeSet<String>),
    NotIn(String, BTreeSet<String>),
    Exists(String),
    NotExists(String),
}

impl<K, V> From<(K, V)> for Expression
where
    K: ToString,
    V: ToString,
{
    fn from(tuple: (K, V)) -> Self {
        Self::Eq(tuple.0.to_string(), tuple.1.to_string())
    }
}

fn join_values(values: &BTreeSet<String>) -> String {
    let mut out = String::with_capacity(values.iter().map(|v| v.len() + 1).sum());
    let mut iter = values.iter().peekable();
    while let Some(value) = iter.next() {
        out.push_str(value);
        if iter.peek().is_some() {
            out.push(',');
        }
    }
    out
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eq(key, value) => write!(f, "{key}={value}"),
            Self::Neq(key, value) => write!(f, "{key}!={value}"),
            Self::In(key, values) => write!(f, "{key} in ({items})", items = join_values(values)),
            Self::NotIn(key, values) => {
                write!(f, "{key} notin ({items})", items = join_values(values))
            }
            Self::Exists(key) => write!(f, "{key}"),
            Self::NotExists(key) => write!(f, "!{key}"),
        }
    }
}

#[derive(Default, Clone, Debug)]
pub struct Selector(Vec<Expression>);

impl Selector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(&mut self, expr: impl Into<Expression>) -> &mut Self {
        self.0.push(expr.into());
        self
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Expression> {
        self.0.iter()
    }
}

impl From<&Selector> for Selector {
    fn from(selector: &Selector) -> Self {
        selector.clone()
    }
}

impl From<Expression> for Selector {
    fn from(expr: Expression) -> Self {
        Self(vec![expr])
    }
}

impl From<Vec<Expression>> for Selector {
    fn from(expr: Vec<Expression>) -> Self {
        Self(expr)
    }
}

impl<K, V> From<(K, V)> for Selector
where
    K: ToString,
    V: ToString,
{
    fn from(tuple: (K, V)) -> Self {
        Selector::from(Expression::from(tuple))
    }
}

impl<I> FromIterator<I> for Selector
where
    Expression: From<I>,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = I>,
    {
        Selector(iter.into_iter().map(Expression::from).collect())
    }
}

impl IntoIterator for Selector {
    type Item = Expression;
    type IntoIter = std::vec::IntoIter<Expression>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut iter = self.iter().peekable();
        while let Some(expr) = iter.next() {
            write!(f, "{expr}")?;
            if iter.peek().is_some() {
                write!(f, ",")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Display)]
pub enum WellKnownField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
}
