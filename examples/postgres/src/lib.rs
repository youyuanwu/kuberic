pub mod adapter;
pub mod config;
pub mod data_service;
pub mod instance;
pub mod monitor;
pub mod service;

pub mod proto {
    tonic::include_proto!("pgdata.v1");
}

#[cfg(feature = "testing")]
pub mod testing;
