use winship::host;
use winship::libvirt;
use winship::model::VmConfig;

fn main() {
    let host = host::inspect_host();
    let mut config = VmConfig::default();
    config.windows_iso = "/var/lib/libvirt/images/winship/win10.iso".into();
    config.virtio_iso = "/var/lib/libvirt/images/winship/virtio-win.iso".into();
    config.disk_path = "/var/lib/libvirt/images/winship/winship-windows.qcow2".into();
    config.disk_size_gib = 80;
    config.cpu_cores = 8;
    config.startup_memory_mib = 4096;
    config.maximum_memory_mib = 12288;
    config.pci_devices = vec![
        "0000:01:00.0".into(),
        "0000:01:00.1".into(),
        "0000:00:0d.0".into(),
    ];
    match libvirt::generate_domain_xml(&config, &host.firmware) {
        Ok(xml) => println!("{xml}"),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
