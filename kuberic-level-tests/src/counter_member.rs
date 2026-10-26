#[path = "../../scripts/fixtures/controller-test-util/src/lib.rs"]
pub mod application;

fn main() -> anyhow::Result<()> {
    kuberic_controller::test_util::ReplicaContext::member_main(application::CounterFactory)?;
    Ok(())
}
