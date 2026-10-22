use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const VM_NAME: &str = "winship-windows";
pub const LIBVIRT_URI: &str = "qemu:///system";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct VmConfig {
    pub vm_uuid: String,
    pub windows_iso: String,
    pub virtio_iso: String,
    pub disk_path: String,
    pub disk_size_gib: u16,
    pub cpu_cores: u16,
    pub startup_memory_mib: u32,
    pub maximum_memory_mib: u32,
    pub pci_devices: Vec<String>,
    pub usb_devices: Vec<UsbSelection>,
    pub physical_disks: Vec<String>,
    pub network_mode: NetworkMode,
    pub network_interface: String,
    pub boot_from_cd: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            vm_uuid: String::new(),
            windows_iso: String::new(),
            virtio_iso: String::new(),
            disk_path: String::new(),
            disk_size_gib: 80,
            cpu_cores: 4,
            startup_memory_mib: 4096,
            maximum_memory_mib: 12288,
            pci_devices: Vec::new(),
            usb_devices: Vec::new(),
            physical_disks: Vec::new(),
            network_mode: NetworkMode::Nat,
            network_interface: String::new(),
            boot_from_cd: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    #[default]
    Nat,
    Macvtap,
    Bridge,
}

impl NetworkMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nat => "NAT",
            Self::Macvtap => "macvtap",
            Self::Bridge => "bridge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInterface {
    pub name: String,
    pub kind: String,
    pub state: String,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct UsbSelection {
    pub vendor_id: String,
    pub product_id: String,
    pub bus: u8,
    pub address: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciDevice {
    pub address: String,
    pub vendor_id: String,
    pub device_id: String,
    pub class: u32,
    pub description: String,
    pub driver: Option<String>,
    pub iommu_group: Option<u32>,
}

impl PciDevice {
    pub fn is_nvidia(&self) -> bool {
        self.vendor_id.eq_ignore_ascii_case("10de")
    }

    pub fn is_display(&self) -> bool {
        self.class >> 16 == 0x03
    }

    pub fn is_audio(&self) -> bool {
        self.class >> 16 == 0x04
    }

    pub fn is_usb_controller(&self) -> bool {
        self.class >> 16 == 0x0c && (self.class >> 8) & 0xff == 0x03
    }

    pub fn vfio_id(&self) -> String {
        format!("{}:{}", self.vendor_id, self.device_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub vendor_id: String,
    pub product_id: String,
    pub bus: u8,
    pub address: u8,
    pub product: String,
}

impl UsbDevice {
    pub fn selection(&self) -> UsbSelection {
        UsbSelection {
            vendor_id: self.vendor_id.clone(),
            product_id: self.product_id.clone(),
            bus: self.bus,
            address: self.address,
        }
    }

    pub fn display_name(&self) -> String {
        let name = if self.product.is_empty() {
            "USB device".to_string()
        } else {
            self.product.clone()
        };
        format!(
            "{name} ({}:{})",
            self.vendor_id.to_ascii_lowercase(),
            self.product_id.to_ascii_lowercase()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDevice {
    pub path: String,
    pub model: String,
    pub size: String,
    pub transport: String,
    pub removable: bool,
    pub mounted: bool,
    pub mountpoints: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FirmwareInfo {
    pub code: Option<PathBuf>,
    pub vars: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct HostSnapshot {
    pub debian_release: String,
    pub kernel_cmdline: String,
    pub iommu_cmdline_enabled: bool,
    pub secure_boot_enabled: Option<bool>,
    pub iommu_groups: Vec<(u32, Vec<String>)>,
    pub pci_devices: Vec<PciDevice>,
    pub usb_devices: Vec<UsbDevice>,
    pub block_devices: Vec<BlockDevice>,
    pub network_interfaces: Vec<NetworkInterface>,
    pub firmware: FirmwareInfo,
}

impl HostSnapshot {
    pub fn iommu_enabled(&self) -> bool {
        !self.iommu_groups.is_empty()
    }

    pub fn iommu_hint(&self) -> &'static str {
        let cpu_vendor = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        if cpu_vendor.contains("GenuineIntel") {
            "intel_iommu=on iommu=pt"
        } else {
            "amd_iommu=on iommu=pt"
        }
    }

    pub fn group_members(&self, group: Option<u32>) -> &[String] {
        group
            .and_then(|id| {
                self.iommu_groups
                    .iter()
                    .find(|(group_id, _)| *group_id == id)
            })
            .map(|(_, members)| members.as_slice())
            .unwrap_or(&[])
    }

    pub fn selected_group_is_complete(&self, config: &VmConfig, group: Option<u32>) -> bool {
        let members = self.group_members(group);
        !members.is_empty()
            && members
                .iter()
                .all(|member| config.pci_devices.contains(member))
    }

    pub fn selected_vfio_ids(&self, config: &VmConfig) -> Result<Vec<String>, ConfigError> {
        let mut ids = Vec::new();
        for address in &config.pci_devices {
            let device = self
                .pci_devices
                .iter()
                .find(|device| device.address == *address)
                .ok_or_else(|| ConfigError::UnknownPciAddress(address.clone()))?;
            let id = device.vfio_id();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    pub fn selected_vfio_conflicts(&self, config: &VmConfig) -> Result<Vec<String>, ConfigError> {
        let ids = self.selected_vfio_ids(config)?;
        Ok(self
            .pci_devices
            .iter()
            .filter(|device| {
                !config.pci_devices.contains(&device.address) && ids.contains(&device.vfio_id())
            })
            .map(|device| device.address.clone())
            .collect())
    }

    pub fn validate_pci_passthrough(&self, config: &VmConfig) -> Result<(), ConfigError> {
        for address in &config.pci_devices {
            let device = self
                .pci_devices
                .iter()
                .find(|device| device.address == *address)
                .ok_or_else(|| ConfigError::UnknownPciAddress(address.clone()))?;
            let group = device
                .iommu_group
                .ok_or_else(|| ConfigError::MissingIommuGroup(address.clone()))?;
            if !self.selected_group_is_complete(config, Some(group)) {
                return Err(ConfigError::IncompleteIommuGroup(group));
            }
        }

        let conflicts = self.selected_vfio_conflicts(config)?;
        if !conflicts.is_empty() {
            return Err(ConfigError::VfioIdConflict(conflicts.join(", ")));
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("VM UUID must be a canonical lowercase UUID")]
    InvalidUuid,
    #[error("A macOS/network interface name is required for macvtap or bridge networking")]
    MissingNetworkInterface,
    #[error("Network interface name contains invalid characters")]
    InvalidNetworkInterface,
    #[error("Startup memory must be greater than zero")]
    StartupMemoryZero,
    #[error("Maximum memory must be at least startup memory")]
    MaximumMemoryBelowStartup,
    #[error("CPU core count must be between 1 and 256")]
    InvalidCpuCount,
    #[error("Disk size must be between 16 and 4096 GiB")]
    InvalidDiskSize,
    #[error("PCI address is invalid: {0}")]
    InvalidPciAddress(String),
    #[error("Unknown PCI address: {0}")]
    UnknownPciAddress(String),
    #[error("PCI device {0} has no IOMMU group and cannot be safely passed through")]
    MissingIommuGroup(String),
    #[error("IOMMU group {0} is incomplete; select every member before passing it through")]
    IncompleteIommuGroup(u32),
    #[error("VFIO vendor:device IDs also match unselected PCI devices: {0}")]
    VfioIdConflict(String),
    #[error(
        "USB identity must include four-digit hexadecimal IDs and a non-zero bus/device address"
    )]
    InvalidUsbId,
    #[error("Path must be absolute: {0}")]
    RelativePath(String),
    #[error("Windows installation ISO is not selected")]
    MissingWindowsIso,
    #[error("VirtIO driver ISO is not selected")]
    MissingVirtioIso,
    #[error("Virtual disk path is not selected")]
    MissingDiskPath,
    #[error("Virtual disk path already exists; choose a new file")]
    DiskAlreadyExists,
}

impl VmConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.vm_uuid.is_empty() && !is_uuid(&self.vm_uuid) {
            return Err(ConfigError::InvalidUuid);
        }
        if self.startup_memory_mib == 0 {
            return Err(ConfigError::StartupMemoryZero);
        }
        if self.maximum_memory_mib < self.startup_memory_mib {
            return Err(ConfigError::MaximumMemoryBelowStartup);
        }
        if self.cpu_cores == 0 || self.cpu_cores > 256 {
            return Err(ConfigError::InvalidCpuCount);
        }
        if !(16..=4096).contains(&self.disk_size_gib) {
            return Err(ConfigError::InvalidDiskSize);
        }
        for address in &self.pci_devices {
            if parse_pci_address(address).is_none() {
                return Err(ConfigError::InvalidPciAddress(address.clone()));
            }
        }
        for selection in &self.usb_devices {
            if !is_hex_id(&selection.vendor_id)
                || !is_hex_id(&selection.product_id)
                || selection.bus == 0
                || selection.address == 0
            {
                return Err(ConfigError::InvalidUsbId);
            }
        }
        for path in [&self.windows_iso, &self.virtio_iso, &self.disk_path]
            .into_iter()
            .filter(|path| !path.is_empty())
        {
            if !Path::new(path).is_absolute() {
                return Err(ConfigError::RelativePath(path.clone()));
            }
        }
        for path in &self.physical_disks {
            if !Path::new(path).is_absolute() {
                return Err(ConfigError::RelativePath(path.clone()));
            }
        }
        if self.network_mode != NetworkMode::Nat {
            if self.network_interface.trim().is_empty() {
                return Err(ConfigError::MissingNetworkInterface);
            }
            if !self.network_interface.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
            }) {
                return Err(ConfigError::InvalidNetworkInterface);
            }
        }
        Ok(())
    }

    pub fn validate_for_create(&self) -> Result<(), ConfigError> {
        self.validate()?;
        if self.windows_iso.is_empty() {
            return Err(ConfigError::MissingWindowsIso);
        }
        if self.virtio_iso.is_empty() {
            return Err(ConfigError::MissingVirtioIso);
        }
        if self.disk_path.is_empty() {
            return Err(ConfigError::MissingDiskPath);
        }
        if Path::new(&self.disk_path).exists() {
            return Err(ConfigError::DiskAlreadyExists);
        }
        Ok(())
    }
}

pub fn parse_pci_address(value: &str) -> Option<(u16, u8, u8, u8)> {
    let (domain, rest) = value.split_once(':')?;
    let (bus, rest) = rest.split_once(':')?;
    let (slot, function) = rest.split_once('.')?;
    if domain.len() != 4 || bus.len() != 2 || slot.len() != 2 || function.len() != 1 {
        return None;
    }
    let domain = u16::from_str_radix(domain, 16).ok()?;
    let bus = u8::from_str_radix(bus, 16).ok()?;
    let slot = u8::from_str_radix(slot, 16).ok()?;
    let function = u8::from_str_radix(function, 16).ok()?;
    (slot <= 0x1f && function <= 0x07).then_some((domain, bus, slot, function))
}

pub fn is_hex_id(value: &str) -> bool {
    value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(),
        })
}

pub fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_memory_matches_first_run_profile() {
        let config = VmConfig::default();
        assert_eq!(config.startup_memory_mib, 4096);
        assert_eq!(config.maximum_memory_mib, 12288);
    }

    #[test]
    fn rejects_memory_and_pci_errors() {
        let mut config = VmConfig {
            startup_memory_mib: 8192,
            maximum_memory_mib: 4096,
            ..VmConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(ConfigError::MaximumMemoryBelowStartup)
        );

        config.maximum_memory_mib = 12288;
        config.pci_devices.push("not-a-bdf".into());
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidPciAddress(_))
        ));
    }

    #[test]
    fn validates_canonical_uuids() {
        assert!(is_uuid("8a77e099-efce-417f-b960-1bbb3a0571b5"));
        assert!(!is_uuid("8A77E099-EFCE-417F-B960-1BBB3A0571B5"));
        assert!(!is_uuid("8a77e099efce417fb9601bbb3a0571b5"));
        assert!(!is_uuid("not-a-uuid"));
        let mut config = VmConfig {
            vm_uuid: "NOT-A-UUID".into(),
            ..VmConfig::default()
        };
        assert_eq!(config.validate(), Err(ConfigError::InvalidUuid));
        config.vm_uuid.clear();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn parses_pci_addresses_and_escapes_xml() {
        assert_eq!(parse_pci_address("0000:0a:1f.3"), Some((0, 10, 31, 3)));
        assert_eq!(parse_pci_address("01:00:00.0"), None);
        assert_eq!(parse_pci_address("0000:00:20.0"), None);
        assert_eq!(parse_pci_address("0000:00:00.8"), None);
        assert_eq!(xml_escape("a<&\"b"), "a&lt;&amp;&quot;b");
    }

    #[test]
    fn identifies_pci_classes() {
        let nvidia = PciDevice {
            address: "0000:01:00.0".into(),
            vendor_id: "10de".into(),
            device_id: "2684".into(),
            class: 0x030000,
            description: String::new(),
            driver: None,
            iommu_group: Some(12),
        };
        assert!(nvidia.is_nvidia());
        assert!(nvidia.is_display());
        assert!(!nvidia.is_audio());
        assert_eq!(nvidia.vfio_id(), "10de:2684");

        let usb = PciDevice {
            class: 0x0c0330,
            ..nvidia
        };
        assert!(usb.is_usb_controller());
    }

    #[test]
    fn finds_unselected_devices_that_would_match_a_vfio_id() {
        let selected = PciDevice {
            address: "0000:01:00.0".into(),
            vendor_id: "10de".into(),
            device_id: "2684".into(),
            class: 0x030000,
            description: "GPU".into(),
            driver: None,
            iommu_group: Some(1),
        };
        let duplicate = PciDevice {
            address: "0000:03:00.0".into(),
            iommu_group: Some(3),
            ..selected.clone()
        };
        let host = HostSnapshot {
            pci_devices: vec![selected, duplicate],
            ..HostSnapshot::default()
        };
        let config = VmConfig {
            pci_devices: vec!["0000:01:00.0".into()],
            ..VmConfig::default()
        };
        assert_eq!(
            host.selected_vfio_conflicts(&config).unwrap(),
            vec!["0000:03:00.0"]
        );
    }
}
