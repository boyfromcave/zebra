//! `zebrad` config-specific shared code for the `zebrad` acceptance tests.
//!
//! # Warning
//!
//! Test functions in this file will not be run.
//! This file is only for test library code.

use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use color_eyre::eyre::Result;
use tempfile::TempDir;

use zebra_test::net::random_known_port;
use zebrad::{
    components::{mempool, sync, tracing},
    config::ZebradConfig,
};

/// Returns a config with:
/// - a Zcash listener on an unused port on IPv4 localhost, and
/// - an ephemeral state,
/// - the minimum syncer lookahead limit, and
/// - shorter task intervals, to improve test coverage.
pub fn default_test_config() -> Result<ZebradConfig> {
    const TEST_DURATION: Duration = Duration::from_secs(30);

    let network = zebra_network::Config {
        // The OS automatically chooses an unused port.
        listen_addr: "127.0.0.1:0".parse()?,
        crawl_new_peer_interval: TEST_DURATION,
        ..zebra_network::Config::default()
    };

    let sync = sync::Config {
        // Avoid downloading unnecessary blocks.
        checkpoint_verify_concurrency_limit: sync::MIN_CHECKPOINT_CONCURRENCY_LIMIT,
        ..sync::Config::default()
    };

    let mempool = mempool::Config {
        eviction_memory_time: TEST_DURATION,
        ..mempool::Config::default()
    };

    let consensus = zebra_consensus::Config {
        debug_skip_parameter_preload: true,
        ..zebra_consensus::Config::default()
    };

    let force_use_color = !matches!(
        env::var("ZEBRA_FORCE_USE_COLOR"),
        Err(env::VarError::NotPresent)
    );
    let tracing = tracing::Config {
        force_use_color,
        ..tracing::Config::default()
    };

    let config = ZebradConfig {
        network,
        state: zebra_state::Config::ephemeral(),
        sync,
        mempool,
        consensus,
        tracing,
        ..ZebradConfig::default()
    };

    Ok(config)
}

pub fn persistent_test_config() -> Result<ZebradConfig> {
    let mut config = default_test_config()?;
    config.state.ephemeral = false;
    Ok(config)
}

pub fn testdir() -> Result<TempDir> {
    tempfile::Builder::new()
        .prefix("zebrad_tests")
        .tempdir()
        .map_err(Into::into)
}

/// Get the directory where we have different config files.
pub fn configs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/configs")
}

/// Given a config file name, return full path to it.
pub fn config_file_full_path(config_file: PathBuf) -> PathBuf {
    let path = configs_dir().join(config_file);
    Path::new(&path).into()
}

/// Returns a `zebrad` config with a random known RPC port.
///
/// Set `parallel_cpu_threads` to true to auto-configure based on the number of CPU cores.
pub fn random_known_rpc_port_config(parallel_cpu_threads: bool) -> Result<ZebradConfig> {
    // [Note on port conflict](#Note on port conflict)
    let listen_port = random_known_port();
    let listen_ip = "127.0.0.1".parse().expect("hard-coded IP is valid");
    let zebra_rpc_listener = SocketAddr::new(listen_ip, listen_port);

    // Write a configuration that has the rpc listen_addr option set
    // TODO: split this config into another function?
    let mut config = default_test_config()?;
    config.rpc.listen_addr = Some(zebra_rpc_listener);
    if parallel_cpu_threads {
        // Auto-configure to the number of CPU cores: most users configre this
        config.rpc.parallel_cpu_threads = 0;
    } else {
        // Default config, users who want to detect port conflicts configure this
        config.rpc.parallel_cpu_threads = 1;
    }

    Ok(config)
}
