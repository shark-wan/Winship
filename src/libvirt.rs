use crate::model::{
    ConfigError, FirmwareInfo, LIBVIRT_URI, UsbSelection, VM_NAME, VmConfig, parse_pci_address,
    xml_escape,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LibvirtError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Command(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0} is not a readable file")]
    MissingFile(String),
    #[error("Block device is mounted on the host: {0}")]
    MountedDisk(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PowerState {
    NotDefined,
    ShutOff,
    Running,
    Paused,
    Other(String),
}

impl PowerState {
    pub fn label(&self) -> &str {
        match self {
            Self::NotDefined => "Not defined",
            Self::ShutOff => "Shut off",
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::Other(state) => state,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }
}

#[derive(Debug, Clone)]
pub struct VmStatus {
    pub state: PowerState,
    pub cpu_count: Option<u16>,
    pub cpu_time_ns: Option<u64>,
    pub max_memory_kib: Option<u64>,
    pub used_memory_kib: Option<u64>,
}

impl Default for VmStatus {
    fn default() -> Self {
        Self {
            state: PowerState::NotDefined,
            cpu_count: None,
            cpu_time_ns: None,
            max_memory_kib: None,
            used_memory_kib: None,
        }
    }
}

pub fn generate_domain_xml(
    config: &VmConfig,
    firmware: &FirmwareInfo,
) -> Result<String, LibvirtError> {
    config.validate()?;
    let code = firmware
        .code
        .as_ref()
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            LibvirtError::Command(
                "OVMF_CODE firmware was not found. Install the Debian ovmf package.".into(),
            )
        })?;
    let vars = firmware
        .vars
        .as_ref()
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            LibvirtError::Command(
                "OVMF_VARS template was not found. Install the Debian ovmf package.".into(),
            )
        })?;

    let uuid = if config.vm_uuid.is_empty() {
        String::new()
    } else {
        format!("  <uuid>{}</uuid>\n", xml_escape(&config.vm_uuid))
    };
    let mut xml = format!(
        "<domain type='kvm'>\n  <name>{VM_NAME}</name>\n{uuid}  <memory unit='MiB'>{}</memory>\n  <currentMemory unit='MiB'>{}</currentMemory>\n  <vcpu placement='static'>{}</vcpu>\n  <os>\n    <type arch='x86_64' machine='q35'>hvm</type>\n    <loader readonly='yes' type='pflash'>{}</loader>\n    <nvram template='{}'>/var/lib/libvirt/qemu/nvram/{VM_NAME}_VARS.fd</nvram>\n  </os>\n  <features>\n    <acpi/>\n    <apic/>\n    <hyperv mode='custom'>\n      <relaxed state='on'/>\n      <vapic state='on'/>\n      <spinlocks state='on' retries='8191'/>\n      <vpindex state='on'/>\n      <runtime state='on'/>\n      <synic state='on'/>\n      <stimer state='on'/>\n      <reset state='on'/>\n      <vendor_id state='on' value='Winship'/>\n    </hyperv>\n    <kvm>\n      <hidden state='on'/>\n    </kvm>\n  </features>\n  <cpu mode='host-passthrough' check='none' migratable='off'>\n    <topology sockets='1' dies='1' cores='{}' threads='1'/>\n  </cpu>\n  <clock offset='localtime'>\n    <timer name='hypervclock' present='yes'/>\n  </clock>\n  <on_poweroff>destroy</on_poweroff>\n  <on_reboot>restart</on_reboot>\n  <on_crash>destroy</on_crash>\n  <devices>\n    <emulator>/usr/bin/qemu-system-x86_64</emulator>\n    <controller type='pci' index='0' model='pcie-root'/>\n    <controller type='sata' index='0'/>\n    <controller type='usb' index='0' model='qemu-xhci'/>\n    <disk type='file' device='disk'>\n      <driver name='qemu' type='qcow2' cache='none' discard='unmap'/>\n      <source file='{}'/>\n      <target dev='vda' bus='virtio'/>\n      {disk_boot}\n    </disk>\n    <disk type='file' device='cdrom'>\n      <driver name='qemu' type='raw'/>\n      <source file='{}'/>\n      <target dev='sda' bus='sata'/>\n      <readonly/>\n      {cd_boot}\n    </disk>\n    <disk type='file' device='cdrom'>\n      <driver name='qemu' type='raw'/>\n      <source file='{}'/>\n      <target dev='sdb' bus='sata'/>\n      <readonly/>\n    </disk>\n",
        config.maximum_memory_mib,
        config.startup_memory_mib,
        config.cpu_cores,
        xml_escape(&code.to_string_lossy()),
        xml_escape(&vars.to_string_lossy()),
        config.cpu_cores,
        xml_escape(&config.disk_path),
        xml_escape(&config.windows_iso),
        xml_escape(&config.virtio_iso),
        disk_boot = if config.boot_from_cd {
            "<boot order='2'/>"
        } else {
            "<boot order='1'/>"
        },
        cd_boot = if config.boot_from_cd {
            "<boot order='1'/>"
        } else {
            ""
        },
    );

    xml.push_str("    <video><model type='none'/></video>\n");
    xml.push_str(&network_interface_xml(config)?);

    let guest_buses = guest_pci_buses(&config.pci_devices)?;
    for (slot_key, bus_index) in &guest_buses {
        let chassis = bus_index;
        let root_slot = 0x01 + u32::from(*bus_index);
        let port = 0x0f + u32::from(*bus_index);
        xml.push_str(&format!(
            "    <controller type='pci' index='{bus_index}' model='pcie-root-port'><model name='pcie-root-port'/><target chassis='{chassis}' port='0x{port:02x}'/><address type='pci' domain='0x0000' bus='0x00' slot='0x{root_slot:02x}' function='0x0'/></controller>\n"
        ));
        let _ = slot_key;
    }

    let multifunction_slots = config
        .pci_devices
        .iter()
        .filter_map(|address| {
            let (domain, bus, slot, function) = parse_pci_address(address)?;
            (function > 0).then_some((domain, bus, slot))
        })
        .collect::<std::collections::BTreeSet<_>>();
    for address in &config.pci_devices {
        let (domain, bus, slot, function) = parse_pci_address(address)
            .ok_or_else(|| ConfigError::InvalidPciAddress(address.clone()))?;
        let bus_index = guest_buses[&(domain, bus, slot)];
        let multifunction = if function == 0 && multifunction_slots.contains(&(domain, bus, slot)) {
            " multifunction='on'"
        } else {
            ""
        };
        xml.push_str(&format!(
            "    <hostdev mode='subsystem' type='pci' managed='no'>\n      <driver name='vfio'/>\n      <source><address domain='0x{domain:04x}' bus='0x{bus:02x}' slot='0x{slot:02x}' function='0x{function:x}'/></source>\n      <address type='pci' domain='0x0000' bus='0x{bus_index:02x}' slot='0x00' function='0x{function:x}'{multifunction}/>\n    </hostdev>\n"
        ));
    }

    for (index, usb) in config.usb_devices.iter().enumerate() {
        validate_usb_selection(usb)?;
        xml.push_str(&format!(
            "    <hostdev mode='subsystem' type='usb' managed='yes'><source><vendor id='0x{}'/><product id='0x{}'/><address bus='{}' device='{}'/></source><alias name='usb-{index}'/></hostdev>\n",
            usb.vendor_id.to_ascii_lowercase(),
            usb.product_id.to_ascii_lowercase(),
            usb.bus,
            usb.address,
        ));
    }

    for (index, path) in config.physical_disks.iter().enumerate() {
        if !Path::new(path).is_absolute() {
            return Err(ConfigError::RelativePath(path.clone()).into());
        }
        let target = disk_target(index + 2);
        xml.push_str(&format!(
            "    <disk type='block' device='disk'><driver name='qemu' type='raw' cache='none'/><source dev='{}'/><target dev='{target}' bus='virtio'/><serial>winship-physical-{index}</serial></disk>\n",
            xml_escape(path),
        ));
    }

    xml.push_str(
        "    <memballoon model='virtio'>\n      <stats period='5'/>\n    </memballoon>\n  </devices>\n</domain>\n",
    );
    Ok(xml)
}

fn network_interface_xml(config: &VmConfig) -> Result<String, ConfigError> {
    use crate::model::NetworkMode;
    let mac = stable_guest_mac(&config.vm_uuid)
        .map(|mac| format!("<mac address='{mac}'/>"))
        .unwrap_or_default();
    let interface = match config.network_mode {
        NetworkMode::Nat => {
            format!(
                "    <interface type='network'><source network='default'/>{mac}<model type='virtio'/></interface>\n"
            )
        }
        NetworkMode::Macvtap => {
            if config.network_interface.trim().is_empty() {
                return Err(ConfigError::MissingNetworkInterface);
            }
            format!(
                "    <interface type='direct'><source dev='{}' mode='bridge'/>{mac}<model type='virtio'/></interface>\n",
                xml_escape(&config.network_interface)
            )
        }
        NetworkMode::Bridge => {
            if config.network_interface.trim().is_empty() {
                return Err(ConfigError::MissingNetworkInterface);
            }
            format!(
                "    <interface type='bridge'><source bridge='{}'/>{mac}<model type='virtio'/></interface>\n",
                xml_escape(&config.network_interface)
            )
        }
    };
    Ok(interface)
}

pub fn stable_guest_mac(uuid: &str) -> Option<String> {
    if !crate::model::is_uuid(uuid) {
        return None;
    }
    let mut hash = 0x811c_9dc5u32;
    for byte in uuid.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    let bytes = hash.to_be_bytes();
    Some(format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        bytes[1], bytes[2], bytes[3]
    ))
}

fn guest_pci_buses(addresses: &[String]) -> Result<BTreeMap<(u16, u8, u8), u8>, ConfigError> {
    let mut slots = BTreeMap::new();
    for address in addresses {
        let (domain, bus, slot, _) = parse_pci_address(address)
            .ok_or_else(|| ConfigError::InvalidPciAddress(address.clone()))?;
        if !slots.contains_key(&(domain, bus, slot)) {
            let index = u8::try_from(slots.len() + 1)
                .map_err(|_| ConfigError::InvalidPciAddress(address.clone()))?;
            slots.insert((domain, bus, slot), index);
        }
    }
    Ok(slots)
}

fn disk_target(index: usize) -> String {
    let mut index = index;
    let mut suffix = String::new();
    while index > 0 {
        index -= 1;
        suffix.insert(0, (b'a' + (index % 26) as u8) as char);
        index /= 26;
    }
    format!("vd{suffix}")
}

fn validate_usb_selection(selection: &UsbSelection) -> Result<(), ConfigError> {
    if selection.vendor_id.len() != 4
        || selection.product_id.len() != 4
        || selection.bus == 0
        || selection.address == 0
        || !selection
            .vendor_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !selection
            .product_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ConfigError::InvalidUsbId);
    }
    Ok(())
}

pub fn domain_status() -> Result<VmStatus, LibvirtError> {
    let output = Command::new("virsh")
        .args(["--connect", LIBVIRT_URI, "dominfo", VM_NAME])
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let lower = error.to_ascii_lowercase();
        if lower.contains("domain")
            && (lower.contains("not found") || lower.contains("failed to get"))
        {
            return Ok(VmStatus::default());
        }
        return Err(LibvirtError::Command(error));
    }
    let mut status = parse_dominfo(&String::from_utf8_lossy(&output.stdout));
    if status.state.is_active() {
        if let Ok(stats) = Command::new("virsh")
            .args([
                "--connect",
                LIBVIRT_URI,
                "domstats",
                "--cpu-total",
                "--balloon",
                VM_NAME,
            ])
            .output()
        {
            if stats.status.success() {
                let output = String::from_utf8_lossy(&stats.stdout);
                status.cpu_time_ns = parse_domstats_cpu_time(&output);
                if let Some((current, _)) = parse_domstats_memory(&output) {
                    status.used_memory_kib = Some(current);
                }
            }
        }
    }
    Ok(status)
}

pub fn parse_dominfo(output: &str) -> VmStatus {
    let mut status = VmStatus::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "State" => {
                let normalized = value.to_ascii_lowercase();
                status.state = if normalized.contains("running") {
                    PowerState::Running
                } else if normalized.contains("paused") {
                    PowerState::Paused
                } else if normalized.contains("shut off") || normalized.contains("shutoff") {
                    PowerState::ShutOff
                } else {
                    PowerState::Other(value.to_string())
                };
            }
            "CPU(s)" => status.cpu_count = value.parse().ok(),
            "Max memory" => {
                status.max_memory_kib = value.split_whitespace().next().and_then(|n| n.parse().ok())
            }
            "Used memory" => {
                status.used_memory_kib =
                    value.split_whitespace().next().and_then(|n| n.parse().ok())
            }
            _ => {}
        }
    }
    status
}

pub fn run_control(action: &str) -> Result<String, LibvirtError> {
    if !matches!(action, "start" | "shutdown" | "destroy") {
        return Err(LibvirtError::Command("Unsupported VM operation".into()));
    }
    let output = Command::new("virsh")
        .args(["--connect", LIBVIRT_URI, action, VM_NAME])
        .output()?;
    if !output.status.success() {
        return Err(LibvirtError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn create_qcow2(path: &Path, size_gib: u16) -> Result<(), LibvirtError> {
    if path.exists() {
        return Err(ConfigError::DiskAlreadyExists.into());
    }
    if !path.is_absolute() {
        return Err(ConfigError::RelativePath(path.display().to_string()).into());
    }
    if !(16..=4096).contains(&size_gib) {
        return Err(ConfigError::InvalidDiskSize.into());
    }
    if !path.parent().is_some_and(Path::exists) {
        return Err(LibvirtError::Command(
            "The virtual disk parent directory does not exist".into(),
        ));
    }
    if !path
        .parent()
        .is_some_and(|parent| parent.starts_with("/var/lib/libvirt/images"))
    {
        return Err(LibvirtError::Command(
            "New virtual disks must be created under /var/lib/libvirt/images".into(),
        ));
    }
    crate::privileged::request_create_disk(path, size_gib)
        .map_err(|error| LibvirtError::Command(error.to_string()))?;
    Ok(())
}

pub fn pci_driver(address: &str) -> Option<String> {
    std::fs::read_link(format!("/sys/bus/pci/devices/{address}/driver"))
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
}

pub fn validate_vfio_binding(config: &VmConfig) -> Result<(), LibvirtError> {
    for address in &config.pci_devices {
        if crate::model::parse_pci_address(address).is_none() {
            return Err(ConfigError::InvalidPciAddress(address.clone()).into());
        }
        match pci_driver(address).as_deref() {
            Some("vfio-pci") => {}
            Some(driver) => {
                return Err(LibvirtError::Command(format!(
                    "PCI 设备 {address} 当前由 {driver} 驱动占用，尚未绑定 vfio-pci。请先在“硬件”页面应用 VFIO 配置并重启主机；Winship 不会在虚拟机启动时热解绑正在使用的设备。"
                )));
            }
            None => {
                return Err(LibvirtError::Command(format!(
                    "PCI 设备 {address} 当前没有内核驱动。请应用 VFIO 配置并重启主机，确认设备已绑定 vfio-pci。"
                )));
            }
        }
    }
    Ok(())
}

pub fn domain_uuid() -> Result<Option<String>, LibvirtError> {
    let output = Command::new("virsh")
        .args(["--connect", LIBVIRT_URI, "domuuid", VM_NAME])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let uuid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    Ok(crate::model::is_uuid(&uuid).then_some(uuid))
}

pub fn new_uuid() -> Result<String, LibvirtError> {
    let uuid = std::fs::read_to_string("/proc/sys/kernel/random/uuid")?
        .trim()
        .to_ascii_lowercase();
    if crate::model::is_uuid(&uuid) {
        Ok(uuid)
    } else {
        Err(LibvirtError::Command(
            "Kernel returned an invalid UUID".into(),
        ))
    }
}

pub fn validate_vm_start(config: &VmConfig) -> Result<(), LibvirtError> {
    if !Path::new(&config.disk_path).is_file() {
        return Err(LibvirtError::Command(format!(
            "虚拟磁盘不存在：{}",
            config.disk_path
        )));
    }
    validate_install_media(config)?;
    validate_vfio_binding(config)?;
    validate_physical_disks(config)?;
    Ok(())
}

pub fn validate_install_media(config: &VmConfig) -> Result<(), LibvirtError> {
    for path in [&config.windows_iso, &config.virtio_iso] {
        if !Path::new(path).is_file() {
            return Err(LibvirtError::MissingFile(path.clone()));
        }
    }
    Ok(())
}

pub fn validate_physical_disks(config: &VmConfig) -> Result<(), LibvirtError> {
    for path in &config.physical_disks {
        if !Path::new(path).exists() {
            return Err(LibvirtError::MissingFile(path.clone()));
        }
        if block_device_is_mounted(path)? {
            return Err(LibvirtError::MountedDisk(path.clone()));
        }
    }
    Ok(())
}

pub fn block_device_is_mounted(path: &str) -> Result<bool, LibvirtError> {
    let output = Command::new("lsblk")
        .args(["--json", "--output", "PATH,MOUNTPOINT"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(LibvirtError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let json: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        LibvirtError::Command(format!("Could not inspect block device: {error}"))
    })?;
    fn has_mount(node: &Value) -> bool {
        node.get("mountpoint")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
            || node
                .get("children")
                .and_then(Value::as_array)
                .is_some_and(|children| children.iter().any(has_mount))
    }
    Ok(json
        .get("blockdevices")
        .and_then(Value::as_array)
        .is_some_and(|devices| devices.iter().any(has_mount)))
}

pub fn undefine_domain() -> Result<(), LibvirtError> {
    let output = Command::new("virsh")
        .args(["--connect", LIBVIRT_URI, "undefine", "--nvram", VM_NAME])
        .output()?;
    if !output.status.success() {
        return Err(LibvirtError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

pub fn define_domain(xml: &str) -> Result<(), LibvirtError> {
    let mut child = Command::new("virsh")
        .args(["--connect", LIBVIRT_URI, "define", "/dev/stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(xml.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(LibvirtError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

pub fn parse_domstats_memory(output: &str) -> Option<(u64, u64)> {
    let mut actual = None;
    let mut rss = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("balloon.current=") {
            actual = value.parse().ok();
        } else if let Some(value) = line.strip_prefix("balloon.rss=") {
            rss = value.parse().ok();
        }
    }
    actual.map(|actual| (actual, rss.unwrap_or_default()))
}

pub fn parse_domstats_cpu_time(output: &str) -> Option<u64> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("cpu.time="))
        .and_then(|value| value.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsbSelection;
    use std::fs;

    fn firmware() -> (FirmwareInfo, std::path::PathBuf) {
        static NEXT_FIXTURE: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let fixture_id = NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("winship-xml-{}-{fixture_id}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let code = root.join("OVMF_CODE_4M.fd");
        let vars = root.join("OVMF_VARS_4M.fd");
        fs::write(&code, "firmware").unwrap();
        fs::write(&vars, "vars").unwrap();
        (
            FirmwareInfo {
                code: Some(code),
                vars: Some(vars),
            },
            root,
        )
    }

    #[test]
    fn emits_valid_q35_ovmf_balloon_and_device_xml() {
        let (firmware, root) = firmware();
        let mut config = VmConfig {
            windows_iso: root.join("windows.iso").to_string_lossy().into_owned(),
            virtio_iso: root.join("virtio.iso").to_string_lossy().into_owned(),
            disk_path: root.join("guest.qcow2").to_string_lossy().into_owned(),
            pci_devices: vec![
                "0000:01:00.0".into(),
                "0000:01:00.1".into(),
                "0000:02:00.0".into(),
            ],
            ..VmConfig::default()
        };
        config.usb_devices.push(UsbSelection {
            vendor_id: "1234".into(),
            product_id: "abcd".into(),
            bus: 3,
            address: 7,
        });
        config
            .physical_disks
            .push("/dev/disk/by-id/usb-Test_Disk".into());

        let xml = generate_domain_xml(&config, &firmware).unwrap();
        let document = roxmltree::Document::parse(&xml).unwrap();
        let root_node = document.root_element();
        assert_eq!(root_node.tag_name().name(), "domain");
        assert_eq!(root_node.attribute("type"), Some("kvm"));
        assert!(xml.contains("machine='q35'"));
        assert!(xml.contains("<memory unit='MiB'>12288</memory>"));
        assert!(!xml.contains("<uuid>"));
        assert!(xml.contains("<currentMemory unit='MiB'>4096</currentMemory>"));
        assert!(xml.contains("<memballoon model='virtio'>"));
        assert!(xml.contains("<stats period='5'/>"));
        assert!(xml.contains("<hyperv mode='custom'>"));
        assert!(xml.contains("<hidden state='on'/>"));
        assert!(!xml.contains("<graphics"));
        assert!(xml.contains("type='pci' managed='no'"));
        assert!(xml.contains("model='pcie-root-port'"));
        assert!(xml.contains("<video><model type='none'/></video>"));
        assert!(xml.contains("<interface type='network'><source network='default'/><model type='virtio'/></interface>"));
        assert!(xml.contains("<boot order='1'/>"));
        assert!(xml.contains("<boot order='2'/>"));
        assert!(xml.contains("function='0x0' multifunction='on'"));
        assert!(xml.contains("type='block'"));
        assert!(xml.contains("<address bus='3' device='7'/>"));
        assert!(xml.contains("/dev/disk/by-id/usb-Test_Disk"));
        assert!(xml.contains("<vendor id='0x1234'/><product id='0xabcd'/>"));
        assert_eq!(xml.matches("model='pcie-root-port'").count(), 2);
        assert!(xml.contains("<target dev='vdb' bus='virtio'/>"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn escapes_paths_and_rejects_bad_usb_ids() {
        let (firmware, root) = firmware();
        let mut config = VmConfig {
            windows_iso: root.join("win&11.iso").to_string_lossy().into_owned(),
            virtio_iso: root.join("virtio.iso").to_string_lossy().into_owned(),
            disk_path: root.join("disk&data.qcow2").to_string_lossy().into_owned(),
            ..VmConfig::default()
        };
        config.vm_uuid = "8a77e099-efce-417f-b960-1bbb3a0571b5".into();
        let xml = generate_domain_xml(&config, &firmware).unwrap();
        assert!(xml.contains("<uuid>8a77e099-efce-417f-b960-1bbb3a0571b5</uuid>"));
        let mac = stable_guest_mac(&config.vm_uuid).unwrap();
        assert!(xml.contains(&format!("<mac address='{mac}'/>")));
        assert_eq!(mac, stable_guest_mac(&config.vm_uuid).unwrap());
        assert!(xml.contains("win&amp;11.iso"));
        assert!(xml.contains("disk&amp;data.qcow2"));
        config.network_mode = crate::model::NetworkMode::Macvtap;
        config.network_interface = "wlp0s20f3".into();
        let xml = generate_domain_xml(&config, &firmware).unwrap();
        assert!(xml.contains("<interface type='direct'><source dev='wlp0s20f3' mode='bridge'/>"));
        assert!(xml.contains(&format!("<mac address='{mac}'/>")));
        config.usb_devices.push(UsbSelection {
            vendor_id: "x".into(),
            product_id: "1234".into(),
            ..UsbSelection::default()
        });
        assert!(matches!(
            generate_domain_xml(&config, &firmware),
            Err(LibvirtError::Config(ConfigError::InvalidUsbId))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_power_and_memory_stats() {
        let status = parse_dominfo(
            "State: running\nCPU(s): 8\nMax memory: 12582912 KiB\nUsed memory: 4194304 KiB\n",
        );
        assert_eq!(status.state, PowerState::Running);
        assert_eq!(status.cpu_count, Some(8));
        assert_eq!(status.max_memory_kib, Some(12_582_912));
        assert_eq!(status.used_memory_kib, Some(4_194_304));
        assert_eq!(
            parse_domstats_memory("balloon.current=4194304\nballoon.rss=4170000\n"),
            Some((4_194_304, 4_170_000))
        );
        assert_eq!(
            parse_domstats_cpu_time("Domain: winship-windows\ncpu.time=5000000000\n"),
            Some(5_000_000_000)
        );
    }

    #[test]
    fn numbers_physical_disk_targets() {
        assert_eq!(disk_target(1), "vda");
        assert_eq!(disk_target(2), "vdb");
        assert_eq!(disk_target(27), "vdaa");
    }
}
