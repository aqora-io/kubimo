use clap::Parser;
use k8s_openapi::List;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(long)]
    helm_if: Option<String>,
    #[arg(long)]
    compact: bool,
}

fn main() {
    let args = Args::parse();
    let list = List {
        items: kubimo::all_crds(),
        ..Default::default()
    };
    let mut json = if args.compact {
        serde_json::to_string(&list)
    } else {
        serde_json::to_string_pretty(&list)
    }
    .unwrap();
    if let Some(helm_if) = args.helm_if {
        let nl = if args.compact { "" } else { "\n" };
        json = format!("{{{{- if {helm_if} }}}}{nl}{json}{nl}{{{{- end }}}}");
    }
    println!("{json}");
}
