fn main() -> anyhow::Result<()> {
    use downstream_controller_test_util::retained_witness::WitnessFactory;
    kuberic_controller::test_util::ReplicaContext::member_main(WitnessFactory::new(
        downstream_controller_test_util::CounterFactory,
        std::env::current_exe()?,
    ))?;
    Ok(())
}
