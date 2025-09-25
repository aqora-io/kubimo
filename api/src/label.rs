use std::borrow::Cow;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KubimoLabel<'a>(Cow<'a, str>);

impl<'a> KubimoLabel<'a> {
    pub fn new(name: impl ToString) -> KubimoLabel<'static> {
        KubimoLabel(Cow::Owned(name.to_string()))
    }

    pub const fn borrow(name: &'a str) -> Self {
        Self(Cow::Borrowed(name))
    }
}

impl fmt::Display for KubimoLabel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "kubimo.aqora.io/{}", self.0)
    }
}
