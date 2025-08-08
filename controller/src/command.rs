pub struct Command(Vec<String>);

impl Command {
    pub fn new(command: impl IntoIterator<Item = impl ToString>) -> Self {
        Self(
            command
                .into_iter()
                .map(|item| item.to_string())
                .collect::<Vec<_>>(),
        )
    }

    pub fn fmt(command: impl IntoIterator<Item = impl ToString>) -> Vec<String> {
        Self::new(command).0
    }
}

impl From<Command> for Vec<String> {
    fn from(value: Command) -> Self {
        value.0
    }
}
