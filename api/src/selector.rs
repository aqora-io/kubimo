use std::fmt;

#[derive(Clone, Debug)]
pub enum Expression {
    Eq(String, String),
    Neq(String, String),
}

impl Expression {
    pub fn eq(key: impl ToString, value: impl ToString) -> Self {
        Self::Eq(key.to_string(), value.to_string())
    }

    pub fn neq(key: impl ToString, value: impl ToString) -> Self {
        Self::Neq(key.to_string(), value.to_string())
    }
}

impl<K, V> From<(K, V)> for Expression
where
    K: ToString,
    V: ToString,
{
    fn from(tuple: (K, V)) -> Self {
        Self::eq(tuple.0, tuple.1)
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eq(key, value) => write!(f, "{key}={value}"),
            Self::Neq(key, value) => write!(f, "{key}!={value}"),
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
