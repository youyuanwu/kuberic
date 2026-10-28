mod child;
mod parent;
mod shared;

#[allow(dead_code)]
#[path = "../../controller-test-util/src/lib.rs"]
mod counter;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = if args.first().map(String::as_str) == Some("child") {
        child::run(args.into_iter()).await
    } else {
        parent::run(args).await
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ordinary21 owner recovery failed: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
