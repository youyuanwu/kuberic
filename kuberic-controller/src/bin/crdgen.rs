use kube::CustomResourceExt;
use kuberic_controller::crd::KubericSet;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = format!("{}\n", serde_json::to_string_pretty(&KubericSet::crd())?);
    if let Some(path) = std::env::args_os().nth(1) {
        std::fs::write(path, schema)?;
    } else {
        print!("{schema}");
    }
    Ok(())
}
