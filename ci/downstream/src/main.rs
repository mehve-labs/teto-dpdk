use teto_dpdk::FStackConfig;
use teto_tokio::{TetoRuntime, TetoTcpListener};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let config = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config.ini");
    let cfg = FStackConfig::new(config)
        .with_eal_arg("--vdev=net_tap0,iface=dtap0,mac=fixed")
        .with_eal_arg("--no-pci")
        .with_eal_arg("--iova-mode=va")
        .capture_init_output(true);
    let rt = TetoRuntime::start(cfg).await?;
    let listener = TetoTcpListener::bind(&rt, "0.0.0.0:8080".parse().unwrap(), Default::default()).await?;
    println!("downstream crate linked and started F-Stack; listening on {}", listener.local_addr());
    Ok(())
}
