use std::io::stdout;

use k8s_openapi::List;
use kube::CustomResourceExt;
use kubimo::{Exporter, Runner, Workspace};

fn main() {
    let items = vec![Workspace::crd(), Runner::crd(), Exporter::crd()];
    serde_json::to_writer_pretty(
        stdout(),
        &List {
            items,
            ..Default::default()
        },
    )
    .unwrap()
}
