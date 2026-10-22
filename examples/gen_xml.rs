use winship::host;
use winship::libvirt;
use winship::model::VmConfig;

fn main() {
    let host = host::inspect_host();
    let config = VmConfig {
        windows_iso: "/var/lib/libvirt/images/winship/win10.iso".into(),
        virtio_iso: "/var/lib/libvirt/images/winship/virtio-win.iso".into(),
        disk_path: "/var/lib/libvirt/images/winship/winship-windows.qcow2".into(),
        disk_size_gib: 80,
        cpu_cores: 8,
        startup_memory_mib: 4096,
        maximum_memory_mib: 12288,
        vm_uuid: std::env::var("WINSHIP_UUID").unwrap_or_default(),
        pci_devices: vec![
            "0000:01:00.0".into(),
            "0000:01:00.1".into(),
            "0000:00:0d.0".into(),
        ],
        ..VmConfig::default()
    };
    match libvirt::generate_domain_xml(&config, &host.firmware) {
        Ok(xml) => println!("{xml}"),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
