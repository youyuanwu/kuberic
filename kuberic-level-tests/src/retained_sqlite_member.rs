pub mod retained_applications;
#[path = "../../scripts/fixtures/controller-test-util/src/retained_witness.rs"]
pub mod retained_witness;

fn main() -> anyhow::Result<()> {
    kuberic_controller::test_util::ReplicaContext::member_main(
        retained_witness::WitnessFactory::new(
            retained_applications::SqliteFactory,
            std::env::current_exe()?,
        ),
    )?;
    Ok(())
}
