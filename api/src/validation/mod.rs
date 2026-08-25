use kube::core::Rule;

pub fn budget_selector_not_empty() -> Rule {
    Rule::new(include_str!("./budget_selector_not_empty.cel"))
        .message("budget selector must not be empty")
        .field_path(".spec.selector")
}

pub fn workspace_restore_from_not_indexer_prefix() -> Rule {
    Rule::new(include_str!(
        "./workspace_restore_from_not_indexer_prefix.cel"
    ))
    .message("the workspace's own indexer must not write to the restoreFrom archive location")
    .field_path(".spec.restoreFrom")
}

pub fn workspace_immutable_fields() -> Rule {
    Rule::new(include_str!("./workspace_immutable_fields.cel"))
        .message("workspace is immutable")
        .field_path(".spec.pythonRuntime")
}

pub fn runner_immutable_fields() -> Rule {
    Rule::new(include_str!("./runner_immutable_fields.cel"))
        .message("workspace is immutable")
        .field_path(".spec.workspace")
}

/// Render is excluded from pools: a renderer's slot is bound read-only at
/// publish time, but a warm pod's anonymous slot is published read-write long
/// before any Runner exists, so a pooled renderer would lose that guarantee.
pub fn pool_command_not_render() -> Rule {
    Rule::new(include_str!("./pool_command_not_render.cel"))
        .message("pool command must be Edit or Run")
        .field_path(".spec.command")
}

/// Conda is excluded from pools for now. A cold conda runner blocks on
/// `mamba_update_workspace` *before* marimo serves; a pre-booted pool pod
/// would have to run it after the claim, mutating the environment underneath
/// kernels a user may already be connecting to.
pub fn pool_python_runtime_uv() -> Rule {
    Rule::new(include_str!("./pool_python_runtime_uv.cel"))
        .message("pools only support the Uv python runtime")
        .field_path(".spec.pythonRuntime")
}

/// A warm pod's image and venv template are baked at creation; a pool that
/// changed runtime or command in place would claim pods built for the old
/// spec. Replicas, resources and sidecars may change — the pool controller
/// retires drifted warm pods — but these two identify what a pod *is*.
pub fn pool_immutable_fields() -> Rule {
    Rule::new(include_str!("./pool_immutable_fields.cel"))
        .message("pool command and pythonRuntime are immutable")
        .field_path(".spec.command")
}

/// See [`runner_max_memory_greater_than_min`].
pub fn pool_max_memory_greater_than_min() -> Rule {
    Rule::new(include_str!("./pool_max_memory_greater_than_min.cel"))
        .message("pool max memory must be greater than or equal to min memory")
        .field_path(".spec.memory.max")
}

/// See [`runner_max_memory_greater_than_min`].
pub fn pool_max_cpu_greater_than_min() -> Rule {
    Rule::new(include_str!("./pool_max_cpu_greater_than_min.cel"))
        .message("pool max cpu must be greater than or equal to min cpu")
        .field_path(".spec.cpu.max")
}

/// `max >= min`, expressed as "min is not greater than max".
///
/// Written that way because the quantity library offers only `isGreaterThan`
/// and `isLessThan`, with no `>=`. Comparing `max.isGreaterThan(min)` instead
/// would be strict, which contradicts the message: a runner pinned to an
/// exact size — `min == max`, which is how a runner that must not burst is
/// written, and what a platform that sizes requests and limits alike produces
/// — was refused at admission by a rule whose message promised "greater than
/// or equal to".
pub fn runner_max_memory_greater_than_min() -> Rule {
    Rule::new(include_str!("./runner_max_memory_greater_than_min.cel"))
        .message("runner max memory must be greater than or equal to min memory")
        .field_path(".spec.memory.max")
}

/// See [`runner_max_memory_greater_than_min`].
pub fn runner_max_cpu_greater_than_min() -> Rule {
    Rule::new(include_str!("./runner_max_cpu_greater_than_min.cel"))
        .message("runner max cpu must be greater than or equal to min cpu")
        .field_path(".spec.cpu.max")
}

pub fn log_level() -> Rule {
    Rule::new(include_str!("./log_level.cel"))
        .message("logLevel must be one of: Debug, Info, Warn, Error, Critical")
        .field_path(".spec.logLevel")
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
        test_compiles(workspace_restore_from_not_indexer_prefix());
        test_compiles(budget_selector_not_empty());
        test_compiles(workspace_immutable_fields());
        test_compiles(runner_immutable_fields());
        test_compiles(runner_max_memory_greater_than_min());
        test_compiles(runner_max_cpu_greater_than_min());
        test_compiles(pool_command_not_render());
        test_compiles(pool_python_runtime_uv());
        test_compiles(pool_immutable_fields());
        test_compiles(pool_max_memory_greater_than_min());
        test_compiles(pool_max_cpu_greater_than_min());
        test_compiles(log_level());
    }
}
