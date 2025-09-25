use std::io::stdout;

use k8s_openapi::List;

fn main() {
    serde_json::to_writer_pretty(
        stdout(),
        &List {
            items: kubimo::all_crds(),
            ..Default::default()
        },
    )
    .unwrap()
}
