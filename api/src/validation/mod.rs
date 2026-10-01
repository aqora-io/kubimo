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

/// A workspace's pods, pool claims and notebook environments are all built for
/// its runtime's sandbox backend, so the runtime cannot change in place.
/// Compared resolved, absent being `Uv`: under server-side apply, a client that
/// starts or stops sending the default changes nothing and is not refused.
pub fn workspace_immutable_fields() -> Rule {
    Rule::new(include_str!("./workspace_immutable_fields.cel"))
        .message("workspace pythonRuntime is immutable")
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

/// A warm pod's command and sandbox backend are baked at creation, so a pool
/// that changed either in place would claim pods built for the old spec;
/// replicas, resources and sidecars may change (the pool controller retires
/// drifted warm pods). The runtime is compared resolved, as on a Workspace.
pub fn pool_immutable_fields() -> Rule {
    Rule::new(include_str!("./pool_immutable_fields.cel"))
        .message("pool command and pythonRuntime are immutable")
        .field_path(".spec.command")
}

/// Warm pods are named `<pool>-<8 hex>`, and each is routed through a Service
/// named after it, so the pool name plus that suffix must be a DNS-1035 label.
pub fn pool_name_is_a_service_name() -> Rule {
    Rule::new(include_str!("./pool_name_is_a_service_name.cel"))
        .message("pool name must be a DNS-1035 label of at most 54 characters")
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
        test_compiles(workspace_immutable_fields());
        test_compiles(budget_selector_not_empty());
        test_compiles(runner_immutable_fields());
        test_compiles(runner_max_memory_greater_than_min());
        test_compiles(runner_max_cpu_greater_than_min());
        test_compiles(pool_command_not_render());
        test_compiles(pool_immutable_fields());
        test_compiles(pool_max_memory_greater_than_min());
        test_compiles(pool_max_cpu_greater_than_min());
        test_compiles(log_level());
        test_compiles(pool_name_is_a_service_name());
    }

    /// Warm pods are named `<pool>-<8 hex>`, and each pod's Service after the
    /// pod, so the pool name must leave room for a valid Service name.
    #[test]
    fn pool_names_leave_room_for_their_warm_pods_service_names() {
        use cel_interpreter::{Context, Value};
        let program = Program::compile(&pool_name_is_a_service_name().rule).unwrap();
        let accepts = |name: &str| {
            let mut ctx = Context::default();
            ctx.add_variable("self", serde_json::json!({"metadata": {"name": name}}))
                .unwrap();
            program.execute(&ctx).unwrap() == Value::Bool(true)
        };
        assert!(accepts("runner"));
        assert!(accepts("runner-conda-view"));
        assert!(accepts(&"a".repeat(54)));
        assert!(!accepts(&"a".repeat(55)));
        assert!(!accepts("1runner"));
        assert!(!accepts("runner.view"));
        assert!(!accepts("runner-"));
    }
}
