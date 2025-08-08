macro_rules! cmd {
    ($($part:literal),*$(,)?) => {
        vec![$(format!($part)),*]
    }
}

pub(crate) use cmd;
