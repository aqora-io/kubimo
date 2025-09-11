use kube::core::Rule;

pub fn runner_immutable_fields() -> Rule {
    Rule::new(include_str!("./runner_immutable_fields.cel"))
        .message("workspace is immutable")
        .field_path(".spec.workspace")
}

pub fn workspace_max_storage_greater_than_min() -> Rule {
    Rule::new(include_str!("./workspace_max_storage_greater_than_min.cel"))
        .message("workspace max storage must be greater than or equal to min storage")
        .field_path(".spec.storage.max")
}

pub fn runner_max_memory_greater_than_min() -> Rule {
    Rule::new(include_str!("./runner_max_memory_greater_than_min.cel"))
        .message("runner max memory must be greater than or equal to min memory")
        .field_path(".spec.memory.max")
}

pub fn runner_max_cpu_greater_than_min() -> Rule {
    Rule::new(include_str!("./runner_max_cpu_greater_than_min.cel"))
        .message("runner max cpu must be greater than or equal to min cpu")
        .field_path(".spec.cpu.max")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel_interpreter::Program;

    fn test_compiles(rule: Rule) {
        if let Err(e) = Program::compile(&rule.rule) {
            panic!("{e}")
        }
    }

    #[test]
    fn test_runner_cel_compiles() {
        test_compiles(runner_immutable_fields());
        test_compiles(workspace_max_storage_greater_than_min());
        test_compiles(runner_max_memory_greater_than_min());
        test_compiles(runner_max_cpu_greater_than_min());
    }
}
