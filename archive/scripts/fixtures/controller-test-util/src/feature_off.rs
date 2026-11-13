use kuberic_controller::test_util::TestCluster;

fn main() {
    let _ = std::any::type_name::<TestCluster<()>>();
}
