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

/// The Job is named `<name>-import`, and a Job name becomes a pod label value,
/// which Kubernetes caps at 63 characters. Refusing a longer name at admission
/// beats a Job create that fails forever without a status.
pub fn import_job_name_length() -> Rule {
    Rule::new(include_str!("./import_job_name_length.cel"))
        .message("import job name must be at most 56 characters")
}

/// Exactly one input source per file. `s3` is the only one so far; a new
/// source joins this rule rather than changing the Rust type.
pub fn import_job_input_source() -> Rule {
    Rule::new(include_str!("./import_job_input_source.cel"))
        .message("import job file input must set exactly one source")
        .field_path(".spec.files")
}

/// Somewhere under the workspace root. The importer checks the resolved path
/// again; this keeps an obviously bad one from ever becoming a Job.
pub fn import_job_output_path() -> Rule {
    Rule::new(include_str!("./import_job_output_path.cel"))
        .message(
            "import job output path must be a relative file path without empty, . or .. segments",
        )
        .field_path(".spec.files")
}

/// Two files written to one path would leave only the last of them.
pub fn import_job_unique_output_paths() -> Rule {
    Rule::new(include_str!("./import_job_unique_output_paths.cel"))
        .message("import job output paths must be unique")
        .field_path(".spec.files")
}

/// The importer picks a converted file's input format from its key's
/// extension, the formats `marimo convert` reads.
pub fn import_job_convert_input() -> Rule {
    Rule::new(include_str!("./import_job_convert_input.cel"))
        .message(
            "import job file to convert must have an input key ending with .ipynb, .md, .qmd or .py",
        )
        .field_path(".spec.files")
}

/// `marimo convert` only writes marimo notebooks.
pub fn import_job_convert_output() -> Rule {
    Rule::new(include_str!("./import_job_convert_output.cel"))
        .message("import job file to convert must have an output path ending with .py")
        .field_path(".spec.files")
}

/// An import runs once; its Job is never re-created or updated, so an edited
/// spec would describe something that never happens.
pub fn import_job_immutable() -> Rule {
    Rule::new(include_str!("./import_job_immutable.cel"))
        .message("import job spec is immutable")
        .field_path(".spec")
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
        test_compiles(import_job_name_length());
        test_compiles(import_job_input_source());
        test_compiles(import_job_output_path());
        test_compiles(import_job_unique_output_paths());
        test_compiles(import_job_convert_input());
        test_compiles(import_job_convert_output());
        test_compiles(import_job_immutable());
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

    /// Whether `rule` admits an ImportJob with `files`, each given as
    /// `(key, output path, convert)`. `None` leaves the field out.
    fn admits(rule: &Rule, files: &[(Option<&str>, &str, Option<bool>)]) -> bool {
        use cel_interpreter::{Context, Value};
        let files: Vec<_> = files
            .iter()
            .map(|(key, path, convert)| {
                let mut file = serde_json::json!({
                    "input": {},
                    "output": {"path": path},
                });
                if let Some(key) = key {
                    file["input"]["s3"] =
                        serde_json::json!({"bucket": "b", "key": key, "secretName": "s"});
                }
                if let Some(convert) = convert {
                    file["convert"] = serde_json::json!(convert);
                }
                file
            })
            .collect();
        let mut ctx = Context::default();
        ctx.add_variable("self", serde_json::json!({"spec": {"files": files}}))
            .unwrap();
        Program::compile(&rule.rule).unwrap().execute(&ctx).unwrap() == Value::Bool(true)
    }

    #[test]
    fn import_job_files_need_a_source() {
        let rule = import_job_input_source();
        assert!(admits(&rule, &[(Some("a.csv"), "a.csv", None)]));
        assert!(!admits(
            &rule,
            &[(Some("a.csv"), "a.csv", None), (None, "b.csv", None)]
        ));
    }

    #[test]
    fn import_job_output_paths_stay_relative() {
        let rule = import_job_output_path();
        for path in ["a.py", "data/My File.csv", "a/.hidden", "-odd.txt", "a..b"] {
            assert!(admits(&rule, &[(Some("k"), path, None)]), "{path}");
        }
        for path in [
            "/abs.py",
            "../a.py",
            "a/../b.py",
            "a/./b.py",
            "a//b.py",
            "./a.py",
            "a/",
            ".",
            "..",
        ] {
            assert!(!admits(&rule, &[(Some("k"), path, None)]), "{path}");
        }
    }

    #[test]
    fn import_job_output_paths_are_unique() {
        let rule = import_job_unique_output_paths();
        assert!(admits(
            &rule,
            &[(Some("a"), "a.py", None), (Some("b"), "b.py", None)]
        ));
        assert!(!admits(
            &rule,
            &[
                (Some("a"), "a.py", None),
                (Some("b"), "b.py", None),
                (Some("c"), "a.py", Some(true)),
            ]
        ));
    }

    /// Only a converted file is held to `marimo convert`'s formats; a copied
    /// one can be anything.
    #[test]
    fn only_converted_files_are_held_to_marimo_convert_formats() {
        let input = import_job_convert_input();
        let output = import_job_convert_output();
        for key in ["a.ipynb", "a.md", "a.qmd", "My Script.py"] {
            assert!(admits(&input, &[(Some(key), "a.py", Some(true))]), "{key}");
        }
        assert!(!admits(&input, &[(Some("a.txt"), "a.py", Some(true))]));
        assert!(!admits(
            &output,
            &[(Some("a.ipynb"), "a.ipynb", Some(true))]
        ));
        for convert in [None, Some(false)] {
            assert!(admits(&input, &[(Some("a.txt"), "a.txt", convert)]));
            assert!(admits(&output, &[(Some("a.ipynb"), "a.ipynb", convert)]));
        }
    }

    /// `convert: null` means what the controller reads it as: absent, a copy.
    #[test]
    fn a_null_convert_is_a_copy() {
        use cel_interpreter::{Context, Value};
        let mut ctx = Context::default();
        let file = serde_json::json!({
            "input": {"s3": {"bucket": "b", "key": "a.txt", "secretName": "s"}},
            "output": {"path": "a.txt"},
            "convert": null,
        });
        ctx.add_variable("self", serde_json::json!({"spec": {"files": [file]}}))
            .unwrap();
        for rule in [import_job_convert_input(), import_job_convert_output()] {
            let result = Program::compile(&rule.rule).unwrap().execute(&ctx);
            assert_eq!(result.unwrap(), Value::Bool(true), "{}", rule.rule);
        }
    }
}
