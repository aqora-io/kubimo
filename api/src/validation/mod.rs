use kube::core::Rule;

/// `max >= min`, expressed as "min is not greater than max".
///
/// Written that way because the quantity library offers only `isGreaterThan`
/// and `isLessThan`, with no `>=`. Comparing `max.isGreaterThan(min)` instead
/// would be strict, which contradicts the message and refuses `min == max` — a
/// legitimate spec for a workspace that does not grow. It did: a `Pooled`
/// workspace, whose slot quota is fixed, was rejected at admission.
pub fn workspace_max_storage_greater_than_min() -> Rule {
    Rule::new(include_str!("./workspace_max_storage_greater_than_min.cel"))
        .message("workspace max storage must be greater than or equal to min storage")
        .field_path(".spec.storage.max")
}

pub fn budget_selector_not_empty() -> Rule {
    Rule::new(include_str!("./budget_selector_not_empty.cel"))
        .message("budget selector must not be empty")
        .field_path(".spec.selector")
}

pub fn workspace_auto_scale_bounds() -> Rule {
    Rule::new(include_str!("./workspace_auto_scale_bounds.cel"))
        .message("storage auto-scale requires 0 < from < 1 and to > 1")
        .field_path(".spec.storage.auto")
}

pub fn workspace_no_volume_with_name() -> Rule {
    Rule::new(include_str!("./workspace_no_volume_with_name.cel"))
        .message("Volume names must not match the resource metadata.name")
        .field_path(".spec.volumes")
}

pub fn workspace_restore_from_exclusive() -> Rule {
    Rule::new(include_str!("./workspace_restore_from_exclusive.cel"))
        .message("restoreFrom and cloneWorkspaceName are mutually exclusive")
        .field_path(".spec.restoreFrom")
}

pub fn workspace_restore_from_not_indexer_prefix() -> Rule {
    Rule::new(include_str!(
        "./workspace_restore_from_not_indexer_prefix.cel"
    ))
    .message("the workspace's own indexer must not write to the restoreFrom archive location")
    .field_path(".spec.restoreFrom")
}

/// A workspace that is Pooled — by spec *or* by materialized status — may never
/// be told it is Dedicated.
///
/// `status.mode` is written on every reconcile path, so on a cluster whose
/// default is Pooled a client-created workspace carries `spec.mode: None` plus
/// `status.mode: Pooled`. Keying only on `oldSelf.spec.mode` therefore admitted
/// `spec.mode: Dedicated` on exactly those objects. `effective_mode` prefers the
/// status, so nothing breaks the moment it is applied — but the object now
/// contradicts itself, and any event that drops the status (a spec-only restore,
/// a status replace, a CRD re-create) resolves it to Dedicated and the
/// controller provisions a PVC and an init Job for a workspace whose files live
/// in a pooled slot.
///
/// Stated as "was Pooled ⇒ the new spec must not say Dedicated" rather than
/// "⇒ must be Pooled": the latter would make every status-only-Pooled workspace
/// unpatchable, since its spec has no mode to satisfy the requirement with.
pub fn workspace_mode_no_downgrade() -> Rule {
    Rule::new(include_str!("./workspace_mode_no_downgrade.cel"))
        .message("workspace mode cannot be changed back from Pooled to Dedicated")
        .field_path(".spec.mode")
}

/// `cloneWorkspaceName` is only implemented for `Dedicated`.
///
/// Its only consumers are the workspace reconciler's `apply_pvc`, which clones
/// the source PVC through a VolumeSnapshot, and `apply_job` — neither of which
/// runs under `Pooled`, where there is no PVC to clone. So the field was
/// accepted and then read by nothing, and the workspace came up with an empty
/// slot: the CR applied, the runner went Ready, and the user's files were simply
/// absent. That cost a workspace its notebook on 2026-07-29, with no error
/// anywhere. Under `Pooled`, cloning is expressed as a `restoreFrom` pointing at
/// the source's archive instead.
///
/// Pooled by *effective* mode, the same resolution `effective_mode` performs:
/// on a cluster whose default is Pooled a client-created workspace says nothing
/// about its mode, and `status.mode` — written on the first reconcile — is the
/// only record of the choice. Keying on `spec.mode` alone therefore accepted a
/// clone on exactly those workspaces, which is the population pooled mode is
/// being rolled out to.
///
/// One window stays uncaught, and cannot be closed here: between the create and
/// the first reconcile there is no `status.mode` and no `spec.mode` either, and
/// CEL cannot see `KUBIMO__DEFAULT_WORKSPACE_MODE`. Setting the mode explicitly
/// is what makes admission decide it — one more reason for clients to do so.
///
/// The rule sees the whole object, so it also refuses the *status* write that
/// would make a workspace Pooled while its spec still names a clone source: the
/// controller cannot materialize the mode, the workspace never goes Ready, and
/// the failure is in the reconcile log. That is the intended outcome — the
/// alternative is the silent empty workspace this rule exists to prevent.
pub fn workspace_clone_not_pooled() -> Rule {
    Rule::new(include_str!("./workspace_clone_not_pooled.cel"))
        .message("cloneWorkspaceName is not supported for Pooled workspaces; use restoreFrom")
        .field_path(".spec.cloneWorkspaceName")
}

pub fn runner_immutable_fields() -> Rule {
    Rule::new(include_str!("./runner_immutable_fields.cel"))
        .message("workspace is immutable")
        .field_path(".spec.workspace")
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
        test_compiles(workspace_max_storage_greater_than_min());
        test_compiles(workspace_auto_scale_bounds());
        test_compiles(workspace_restore_from_exclusive());
        test_compiles(workspace_restore_from_not_indexer_prefix());
        test_compiles(workspace_mode_no_downgrade());
        test_compiles(budget_selector_not_empty());
        test_compiles(workspace_no_volume_with_name());
        test_compiles(workspace_clone_not_pooled());
        test_compiles(runner_immutable_fields());
        test_compiles(runner_max_memory_greater_than_min());
        test_compiles(runner_max_cpu_greater_than_min());
        test_compiles(log_level());
    }
}
