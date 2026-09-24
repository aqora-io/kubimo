use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvFromSource, EnvVar, PodSpec, PodTemplateSpec,
    SecretEnvSource, SecurityContext, Volume, VolumeMount,
};
use kubimo::k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kubimo::kube::api::ObjectMeta;
use kubimo::{ImportJob, Workspace, prelude::*};

use crate::command::cmd;
use crate::config::Config;
use crate::context::Context;
use crate::controllers::runner_pod::sandbox_runtime_class;
use crate::controllers::slot_volume;
use crate::controllers::workspace_affinity;
use crate::resources::Resources;

use super::ImportJobReconciler;

/// The emptyDir the fetch containers stage the files on for the importer.
const INPUT_VOLUME: &str = "input";
const INPUT_DIR: &str = "/input";

/// The input formats of a converted file, by extension: what `marimo convert`
/// reads. Must match the `import_job_convert_input` CEL rule and
/// `/app/kubimo_import.py`.
const CONVERT_EXTENSIONS: [&str; 4] = [".ipynb", ".md", ".qmd", ".py"];

/// Suffixed so it cannot share a name with the CacheJob Job a platform might
/// create under the same resource name.
pub(super) fn job_name(name: &str) -> String {
    format!("{name}-import")
}

impl ImportJobReconciler {
    /// Create the Job once; it is never updated or re-created.
    pub(super) async fn apply_job(
        &self,
        ctx: &Context,
        import_job: &ImportJob,
        workspace: &Workspace,
    ) -> Result<Job, kubimo::Error> {
        let namespace = import_job.require_namespace()?;
        let name = job_name(import_job.name()?);
        let jobs = ctx.api_namespaced::<Job>(namespace);
        if let Some(job) = jobs.get_opt(&name).await? {
            // The suffix makes a collision unlikely, not impossible: a CacheJob
            // named `x-import` owns a Job of the same name, and reporting its
            // outcome as this import's would be a lie.
            let owned = job.metadata.owner_references.iter().flatten().any(|oref| {
                oref.controller == Some(true) && Some(&oref.uid) == import_job.metadata.uid.as_ref()
            });
            if !owned {
                return Err(kubimo::Error::Custom(format!(
                    "Job {name} exists but is not this ImportJob's"
                )));
            }
            return Ok(job);
        }
        jobs.patch(&build_job(&ctx.config, import_job, workspace)?)
            .await
    }
}

/// Fetch into an emptyDir with the credentials, then import from it without
/// them: the importer processes untrusted files, and converting an ipynb
/// resolves its `!pip install` lines with `uv pip compile`.
pub(super) fn build_job(
    config: &Config,
    import_job: &ImportJob,
    workspace: &Workspace,
) -> Result<Job, kubimo::Error> {
    let namespace = import_job.require_namespace()?;
    let workspace_name = &import_job.spec.workspace;

    // Each file's `s3-get` arguments, grouped by the Secret fetching it.
    let mut fetches = BTreeMap::<&str, Vec<String>>::new();
    // The system interpreter carries the marimo fork; `-I` keeps the user site
    // and the workspace venv off its path. Each file follows `--` as
    // `convert|copy SRC DST`.
    let mut command = cmd![
        "/usr/local/bin/python3",
        "-I",
        "/app/kubimo_import.py",
        "--root",
        slot_volume::WORKSPACE_DIR,
        "--",
    ];
    for (index, file) in import_job.spec.files.iter().enumerate() {
        let Some(s3) = file.input.s3.as_ref() else {
            return Err(kubimo::Error::Custom(format!(
                "ImportJob file {index} has no input source"
            )));
        };
        // Staged under its index: only a converted file's extension, which
        // picks the format, carries over from the key.
        let source = if file.converts() {
            let Some(extension) = CONVERT_EXTENSIONS
                .iter()
                .find(|extension| s3.key.ends_with(*extension))
            else {
                return Err(kubimo::Error::Custom(format!(
                    "ImportJob file {index} cannot be converted from {:?}",
                    s3.key
                )));
            };
            format!("{INPUT_DIR}/{index}{extension}")
        } else {
            format!("{INPUT_DIR}/{index}")
        };
        // `--flag=value` throughout, so a key starting with `-` is still a value.
        fetches.entry(&s3.secret_name).or_default().extend([
            format!("--bucket={}", s3.bucket),
            format!("--key={}", s3.key),
            format!("--out={source}"),
        ]);
        let mode = if file.converts() { "convert" } else { "copy" };
        command.extend(cmd![mode, source, file.output.path]);
    }

    // One per Secret, so each holds only its own credentials.
    let fetches = fetches
        .into_iter()
        .enumerate()
        .map(|(index, (secret_name, objects))| Container {
            name: format!("fetch-{index}"),
            image: Some(config.agent_image.clone()),
            args: Some([cmd!["s3-get"], objects].concat()),
            env_from: Some(vec![EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: secret_name.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            volume_mounts: Some(vec![VolumeMount {
                name: INPUT_VOLUME.into(),
                mount_path: INPUT_DIR.into(),
                ..Default::default()
            }]),
            // The agent image runs as root for the DaemonSet; fetching needs
            // none of that, and the importer (uid 1000) must read what this
            // writes.
            security_context: Some(SecurityContext {
                run_as_user: Some(1000),
                run_as_group: Some(1000),
                run_as_non_root: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();

    let import = Container {
        name: "import".into(),
        image: Some(config.marimo_image.clone()),
        command: Some(command),
        // Away from the workspace, so `uv` picks up none of its configuration.
        working_dir: Some(INPUT_DIR.into()),
        // `uv pip compile` would otherwise build sdists (arbitrary code) and
        // fetch whatever `--index-url` or `git+` URL a notebook names. Offline,
        // marimo falls back to the unpinned package names.
        env: Some(vec![
            EnvVar {
                name: "UV_OFFLINE".into(),
                value: Some("1".into()),
                ..Default::default()
            },
            EnvVar {
                name: "UV_NO_BUILD".into(),
                value: Some("1".into()),
                ..Default::default()
            },
        ]),
        resources: Resources::default()
            .cpu(import_job.spec.cpu.clone())
            .memory(import_job.spec.memory.clone())
            .into(),
        volume_mounts: Some(vec![
            VolumeMount {
                name: INPUT_VOLUME.into(),
                mount_path: INPUT_DIR.into(),
                read_only: Some(true),
                ..Default::default()
            },
            VolumeMount {
                name: workspace_name.clone(),
                mount_path: slot_volume::MOUNT_DIR.into(),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };

    // Sandboxed like a runner pod (see `runner_pod`), and co-located with any
    // live runner so the two share the workspace's one slot.
    let pod_spec = PodSpec {
        runtime_class_name: sandbox_runtime_class(),
        automount_service_account_token: Some(false),
        enable_service_links: Some(false),
        affinity: Some(workspace_affinity::workspace_affinity(workspace_name)),
        init_containers: Some(fetches),
        containers: vec![import],
        volumes: Some(vec![
            slot_volume::workspace_volume(
                workspace_name,
                // Writes the imported files into the workspace.
                false,
                slot_volume::SlotSources::from_workspace(Some(workspace)),
            ),
            Volume {
                name: INPUT_VOLUME.into(),
                empty_dir: Some(EmptyDirVolumeSource {
                    size_limit: Some(Quantity("1Gi".into())),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]),
        restart_policy: Some("Never".into()),
        ..Default::default()
    };

    Ok(Job {
        metadata: ObjectMeta {
            name: Some(job_name(import_job.name()?)),
            namespace: Some(namespace.to_string()),
            owner_references: Some(vec![import_job.static_controller_owner_ref()?]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            // A failure is deterministic — a missing object, a notebook that
            // does not convert — and the fetch already retries S3 requests.
            backoff_limit: Some(0),
            // Bounds what a Job never reports as failed: a pod stuck Pending on
            // an unschedulable affinity, or on a Secret that does not exist.
            active_deadline_seconds: Some(600),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(workspace_affinity::workspace_label_map(workspace_name)),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::{
        ImportJobFile, ImportJobInput, ImportJobOutput, ImportJobS3Input, ImportJobSpec,
        WorkspaceIndexer, WorkspaceIndexerPod, WorkspacePythonRuntime, WorkspaceSpec,
    };

    fn file(key: &str, path: &str, convert: Option<bool>, secret_name: &str) -> ImportJobFile {
        ImportJobFile {
            input: ImportJobInput {
                s3: Some(ImportJobS3Input {
                    bucket: "imports".to_string(),
                    key: key.to_string(),
                    secret_name: secret_name.to_string(),
                }),
            },
            output: ImportJobOutput {
                path: path.to_string(),
            },
            convert,
        }
    }

    fn import_job(files: Vec<ImportJobFile>) -> ImportJob {
        let mut import_job = ImportJob::new(
            "bmoij-x",
            ImportJobSpec {
                workspace: "bmow-x".to_string(),
                files,
                ..Default::default()
            },
        );
        import_job.metadata.namespace = Some("ns".to_string());
        import_job.metadata.uid = Some("uid-1".to_string());
        import_job
    }

    fn workspace(python_runtime: Option<WorkspacePythonRuntime>) -> Workspace {
        let mut workspace = Workspace::new(
            "bmow-x",
            WorkspaceSpec {
                python_runtime,
                indexer: Some(WorkspaceIndexer {
                    bucket: Some("archive".to_string()),
                    pod: Some(WorkspaceIndexerPod {
                        env_from: Some(vec![EnvFromSource {
                            secret_ref: Some(SecretEnvSource {
                                name: "workspace-s3".to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        workspace.metadata.namespace = Some("ns".to_string());
        workspace
    }

    fn build(files: Vec<ImportJobFile>) -> Job {
        build_job(
            &Config::test_default(),
            &import_job(files),
            &workspace(None),
        )
        .unwrap()
    }

    /// A notebook to convert and a data file to copy, fetched with one Secret.
    fn notebook_and_data() -> Vec<ImportJobFile> {
        vec![
            file("a/b.ipynb", "out/notebook.py", Some(true), "import-s3"),
            file("a/data.csv", "data/data.csv", None, "import-s3"),
        ]
    }

    fn pod_spec(job: &Job) -> &PodSpec {
        job.spec.as_ref().unwrap().template.spec.as_ref().unwrap()
    }

    fn containers(job: &Job) -> (&[Container], &Container) {
        let pod = pod_spec(job);
        assert_eq!(pod.containers.len(), 1);
        (pod.init_containers.as_deref().unwrap(), &pod.containers[0])
    }

    fn secret_name(container: &Container) -> &str {
        let env_from = container.env_from.as_deref().unwrap();
        assert_eq!(env_from.len(), 1, "{}", container.name);
        &env_from[0].secret_ref.as_ref().unwrap().name
    }

    fn mount<'a>(container: &'a Container, name: &str) -> &'a VolumeMount {
        container
            .volume_mounts
            .iter()
            .flatten()
            .find(|mount| mount.name == name)
            .unwrap_or_else(|| panic!("{} has no {name} mount", container.name))
    }

    /// The point of the fetch/import split.
    #[test]
    fn only_the_fetch_containers_get_the_credentials() {
        let job = build(vec![
            file("a/b.ipynb", "b.py", Some(true), "import-s3"),
            file("c/d.csv", "d.csv", None, "other-s3"),
        ]);
        let (fetches, import) = containers(&job);
        let secrets: Vec<_> = fetches.iter().map(secret_name).collect();
        assert_eq!(secrets, ["import-s3", "other-s3"]);
        assert!(import.env_from.is_none());
        let import_json = serde_json::to_string(import).unwrap();
        assert!(!import_json.contains("AWS_"), "{import_json}");
        assert!(!import_json.contains("secretKeyRef"), "{import_json}");
        let pod_json = serde_json::to_string(pod_spec(&job)).unwrap();
        assert_eq!(pod_json.matches("import-s3").count(), 1, "{pod_json}");
        assert_eq!(pod_json.matches("other-s3").count(), 1, "{pod_json}");
    }

    /// Fetching is per Secret, not per file: files are many, Secrets few.
    #[test]
    fn files_sharing_a_secret_share_a_fetch_container() {
        let job = build(vec![
            file("a.ipynb", "a.py", Some(true), "import-s3"),
            file("b.csv", "b.csv", None, "other-s3"),
            file("c.csv", "c.csv", None, "import-s3"),
        ]);
        let (fetches, _) = containers(&job);
        assert_eq!(fetches.len(), 2);
        assert_eq!(fetches[0].name, "fetch-0");
        assert_eq!(secret_name(&fetches[0]), "import-s3");
        assert_eq!(
            fetches[0].args.as_deref().unwrap(),
            [
                "s3-get",
                "--bucket=imports",
                "--key=a.ipynb",
                "--out=/input/0.ipynb",
                "--bucket=imports",
                "--key=c.csv",
                "--out=/input/2",
            ]
        );
        assert_eq!(fetches[1].name, "fetch-1");
        assert_eq!(
            fetches[1].args.as_deref().unwrap(),
            [
                "s3-get",
                "--bucket=imports",
                "--key=b.csv",
                "--out=/input/1"
            ]
        );
    }

    #[test]
    fn the_pod_is_sandboxed() {
        let job = build(notebook_and_data());
        let pod = pod_spec(&job);
        assert_eq!(pod.runtime_class_name, sandbox_runtime_class());
        assert_eq!(pod.automount_service_account_token, Some(false));
        assert_eq!(pod.enable_service_links, Some(false));
        assert!(
            pod.security_context.is_none(),
            "an fsGroup chowns the volume"
        );
        let (fetches, _) = containers(&job);
        for fetch in fetches {
            let context = fetch.security_context.as_ref().unwrap();
            assert_eq!(context.run_as_user, Some(1000));
            assert_eq!(context.run_as_non_root, Some(true));
        }
    }

    #[test]
    fn the_importer_cannot_fetch_or_build_packages() {
        let job = build(notebook_and_data());
        let (_, import) = containers(&job);
        let env: Vec<_> = import
            .env
            .iter()
            .flatten()
            .map(|var| (var.name.as_str(), var.value.as_deref()))
            .collect();
        assert!(env.contains(&("UV_OFFLINE", Some("1"))));
        assert!(env.contains(&("UV_NO_BUILD", Some("1"))));
        assert_eq!(import.working_dir.as_deref(), Some(INPUT_DIR));
        assert_eq!(
            import.command.as_deref().unwrap(),
            [
                "/usr/local/bin/python3",
                "-I",
                "/app/kubimo_import.py",
                "--root",
                "/home/me/workspace",
                "--",
                "convert",
                "/input/0.ipynb",
                "out/notebook.py",
                "copy",
                "/input/1",
                "data/data.csv",
            ]
        );
    }

    /// A key's name never reaches the importer; only a converted file's
    /// extension does.
    #[test]
    fn the_files_are_staged_on_an_empty_dir() {
        let job = build(vec![
            file("a/My Notebook.qmd", "a.py", Some(true), "import-s3"),
            file("a/My Notebook.qmd", "a.qmd", Some(false), "import-s3"),
        ]);
        let (fetches, import) = containers(&job);
        let args = fetches[0].args.as_deref().unwrap();
        assert!(args.contains(&"--out=/input/0.qmd".to_string()), "{args:?}");
        assert!(args.contains(&"--out=/input/1".to_string()), "{args:?}");
        let command = import.command.as_deref().unwrap();
        assert!(!command.iter().any(|arg| arg.contains("My Notebook")));
        assert_ne!(mount(&fetches[0], INPUT_VOLUME).read_only, Some(true));
        assert_eq!(mount(import, INPUT_VOLUME).read_only, Some(true));
        let volume = pod_spec(&job)
            .volumes
            .iter()
            .flatten()
            .find(|volume| volume.name == INPUT_VOLUME)
            .unwrap();
        assert!(volume.empty_dir.as_ref().unwrap().size_limit.is_some());
    }

    #[test]
    fn the_importer_writes_into_the_workspace_slot() {
        let job = build(notebook_and_data());
        let (fetches, import) = containers(&job);
        let slot = mount(import, "bmow-x");
        assert_eq!(slot.mount_path, slot_volume::MOUNT_DIR);
        assert_ne!(slot.read_only, Some(true));
        assert!(
            fetches
                .iter()
                .flat_map(|fetch| fetch.volume_mounts.iter().flatten())
                .all(|mount| mount.name != "bmow-x"),
            "the fetch containers have no business in the workspace"
        );
        let pod = pod_spec(&job);
        let volume = pod
            .volumes
            .iter()
            .flatten()
            .find(|volume| volume.name == "bmow-x")
            .unwrap();
        let csi = volume.csi.as_ref().unwrap();
        assert_eq!(csi.driver, slot_volume::SLOT_CSI_DRIVER);
        assert_eq!(csi.read_only, Some(false));
        assert_eq!(
            pod.affinity,
            Some(workspace_affinity::workspace_affinity("bmow-x"))
        );
        let labels = job.spec.as_ref().unwrap().template.metadata.as_ref();
        assert_eq!(
            labels.unwrap().labels,
            Some(workspace_affinity::workspace_label_map("bmow-x"))
        );
    }

    #[test]
    fn the_job_runs_once_and_belongs_to_the_import_job() {
        let job = build(notebook_and_data());
        assert_eq!(job.metadata.name.as_deref(), Some("bmoij-x-import"));
        assert_eq!(job.metadata.namespace.as_deref(), Some("ns"));
        let owner = &job.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!(owner.kind, "ImportJob");
        assert_eq!(owner.uid, "uid-1");
        assert_eq!(owner.controller, Some(true));
        let spec = job.spec.as_ref().unwrap();
        assert_eq!(spec.backoff_limit, Some(0));
        assert_eq!(spec.active_deadline_seconds, Some(600));
        assert_eq!(pod_spec(&job).restart_policy.as_deref(), Some("Never"));
    }

    #[test]
    fn images_are_the_agent_and_marimo_images_whatever_the_runtime() {
        let config = Config::test_default();
        for runtime in [None, Some(WorkspacePythonRuntime::Conda)] {
            let job = build_job(
                &config,
                &import_job(notebook_and_data()),
                &workspace(runtime),
            )
            .unwrap();
            let (fetches, import) = containers(&job);
            assert_eq!(
                fetches[0].image.as_deref(),
                Some(config.agent_image.as_str())
            );
            assert_eq!(import.image.as_deref(), Some(config.marimo_image.as_str()));
        }
    }

    #[test]
    fn an_unconvertible_or_missing_input_is_refused() {
        let config = Config::test_default();
        let build = |files| build_job(&config, &import_job(files), &workspace(None));
        assert!(build(vec![file("a/b.txt", "b.py", Some(true), "s3")]).is_err());
        assert!(build(vec![file("a/b.txt", "b.txt", None, "s3")]).is_ok());
        let mut no_source = file("a/b.ipynb", "b.py", Some(true), "s3");
        no_source.input.s3 = None;
        assert!(build(vec![no_source]).is_err());
    }
}
