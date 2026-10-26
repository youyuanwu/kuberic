//! Explicit member executable for the packaged downstream counter fixture.

fn main() -> anyhow::Result<()> {
    kuberic_controller::test_util::ReplicaContext::member_main(
        downstream_controller_test_util::CounterFactory,
    )?;
    Ok(())
}
