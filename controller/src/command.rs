macro_rules! cmd {
    ($($part:expr),*$(,)?) => {
        vec![$($part.to_string()),*]
    }
}

pub(crate) use cmd;
