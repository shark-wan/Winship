use crate::model::{
    BlockDevice, FirmwareInfo, HostSnapshot, NetworkInterface, PciDevice, UsbDevice,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn inspect_host() -> HostSnapshot {
    let kernel_cmdline = fs::read_to_string("/proc/cmdline")
        .unwrap_or_default()
        .trim()
        .to_string();
    let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let debian_release = release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|name| name.trim_matches('"').to_string())
        .unwrap_or_else(|| "Debian host".to_string());

    HostSnapshot {
        iommu_cmdline_enabled: iommu_flag_enabled(&kernel_cmdline),
        kernel_cmdline,
        secure_boot_enabled: detect_secure_boot(Path::new("/sys/firmware/efi/efivars")),
        iommu_groups: discover_iommu_groups(Path::new("/sys/kernel/iommu_groups")),
        pci_devices: discover_pci_devices(Path::new("/sys/bus/pci/devices")),
        usb_devices: discover_usb_devices(Path::new("/sys/bus/usb/devices")),
        block_devices: discover_block_devices(),
        network_interfaces: discover_network_interfaces(Path::new("/sys/class/net")),
        firmware: discover_firmware(Path::new("/usr/share/OVMF")),
        debian_release,
    }
}

pub fn detect_secure_boot(root: &Path) -> Option<bool> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("SecureBoot-")
        {
            continue;
        }
        let contents = fs::read(entry.path()).ok()?;
        return contents.get(4).map(|value| *value == 1);
    }
    None
}

pub fn iommu_flag_enabled(cmdline: &str) -> bool {
    cmdline
        .split_whitespace()
        .any(|arg| matches!(arg, "intel_iommu=on" | "amd_iommu=on"))
}

pub fn discover_iommu_groups(root: &Path) -> Vec<(u32, Vec<String>)> {
    let Ok(groups) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for group_entry in groups.flatten() {
        let Some(group_id) = group_entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(devices) = fs::read_dir(group_entry.path().join("devices")) else {
            continue;
        };
        let mut members = devices
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect::<Vec<_>>();
        members.sort();
        result.push((group_id, members));
    }
    result.sort_by_key(|(group_id, _)| *group_id);
    result
}

pub fn discover_pci_devices(root: &Path) -> Vec<PciDevice> {
    let descriptions = lspci_descriptions();
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(address) = entry.file_name().into_string().ok() else {
            continue;
        };
        let (Some(vendor_id), Some(device_id), Some(class)) = (
            read_hex_id(&path.join("vendor")),
            read_hex_id(&path.join("device")),
            read_hex(&path.join("class")),
        ) else {
            continue;
        };
        let driver = fs::read_link(path.join("driver")).ok().and_then(|target| {
            target
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        });
        let iommu_group = fs::read_link(path.join("iommu_group"))
            .ok()
            .and_then(|target| {
                target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.parse().ok())
            });
        let description =
            descriptions
                .get(&address)
                .cloned()
                .unwrap_or_else(|| match class >> 16 {
                    0x03 => format!("Display controller [{vendor_id}:{device_id}]"),
                    0x04 => format!("Audio device [{vendor_id}:{device_id}]"),
                    0x0c if (class >> 8) & 0xff == 0x03 => {
                        format!("USB controller [{vendor_id}:{device_id}]")
                    }
                    _ => format!("PCI device [{vendor_id}:{device_id}]"),
                });
        devices.push(PciDevice {
            address,
            vendor_id,
            device_id,
            class,
            description,
            driver,
            iommu_group,
        });
    }
    devices.sort_by(|left, right| left.address.cmp(&right.address));
    devices
}

fn lspci_descriptions() -> BTreeMap<String, String> {
    let Ok(output) = Command::new("lspci").arg("-Dnn").output() else {
        return BTreeMap::new();
    };
    if !output.status.success() {
        return BTreeMap::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (address, remainder) = line.split_once(' ')?;
            let description = remainder
                .split_once(':')
                .map(|(_, value)| value.trim())
                .unwrap_or(remainder)
                .split(" [")
                .next()?
                .trim();
            (!description.is_empty()).then(|| (address.to_owned(), description.to_owned()))
        })
        .collect()
}

fn read_hex(path: &Path) -> Option<u32> {
    u32::from_str_radix(
        fs::read_to_string(path)
            .ok()?
            .trim()
            .trim_start_matches("0x"),
        16,
    )
    .ok()
}

fn read_hex_id(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let id = u16::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()?;
    Some(format!("{id:04x}"))
}

pub fn discover_usb_devices(root: &Path) -> Vec<UsbDevice> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let (Ok(vendor_id), Ok(product_id), Ok(bus), Ok(address)) = (
            fs::read_to_string(path.join("idVendor")),
            fs::read_to_string(path.join("idProduct")),
            fs::read_to_string(path.join("busnum")),
            fs::read_to_string(path.join("devnum")),
        ) else {
            continue;
        };
        let (Ok(bus), Ok(address)) = (bus.trim().parse(), address.trim().parse()) else {
            continue;
        };
        let product = fs::read_to_string(path.join("product"))
            .unwrap_or_default()
            .trim()
            .to_string();
        devices.push(UsbDevice {
            vendor_id: vendor_id.trim().to_ascii_lowercase(),
            product_id: product_id.trim().to_ascii_lowercase(),
            bus,
            address,
            product,
        });
    }
    devices.sort_by(|left, right| {
        (
            &left.product,
            left.vendor_id.as_str(),
            left.product_id.as_str(),
        )
            .cmp(&(
                &right.product,
                right.vendor_id.as_str(),
                right.product_id.as_str(),
            ))
    });
    devices
}

pub fn discover_network_interfaces(root: &Path) -> Vec<NetworkInterface> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut interfaces = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        let path = entry.path();
        let state = fs::read_to_string(path.join("operstate"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let kind = if path.join("wireless").exists()
            || path.join("phy80211").exists()
            || fs::symlink_metadata(path.join("phy80211")).is_ok()
        {
            "wireless"
        } else if fs::symlink_metadata(path.join("device")).is_ok() {
            "ethernet"
        } else {
            "other"
        };
        let addresses: Vec<String> = Command::new("ip")
            .args(["-brief", "addr", "show", "dev", &name])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .skip(2)
                    .filter(|value| value.contains('/') || value.contains(':'))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if state == "down" && addresses.is_empty() {
            continue;
        }
        interfaces.push(NetworkInterface {
            name,
            kind: kind.into(),
            state,
            addresses,
        });
    }
    interfaces.sort_by(|left, right| {
        (left.kind != "wireless", &left.name).cmp(&(right.kind != "wireless", &right.name))
    });
    interfaces
}

pub fn active_default_interface() -> Option<NetworkInterface> {
    discover_network_interfaces(Path::new("/sys/class/net"))
        .into_iter()
        .find(|interface| interface.state == "up" && interface.kind == "wireless")
        .or_else(|| {
            discover_network_interfaces(Path::new("/sys/class/net"))
                .into_iter()
                .find(|interface| interface.state == "up")
        })
}

pub fn discover_block_devices() -> Vec<BlockDevice> {
    let Ok(output) = Command::new("lsblk")
        .args([
            "--json",
            "--bytes",
            "--paths",
            "--output",
            "NAME,TYPE,SIZE,MODEL,TRAN,RM,MOUNTPOINTS",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(json) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };
    let Some(rows) = json.get("blockdevices").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let get = |name: &str| row.get(name).and_then(Value::as_str).unwrap_or_default();
            let path = get("name");
            let transport = get("tran");
            let removable = row.get("rm").is_some_and(|value| {
                value.as_bool() == Some(true)
                    || value.as_u64() == Some(1)
                    || value.as_str() == Some("1")
            });
            let size = row
                .get("size")
                .and_then(|value| {
                    value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
                })
                .unwrap_or_default();
            if get("type") != "disk" || path.is_empty() || (transport != "usb" && !removable) {
                return None;
            }
            fn collect_mountpoints(node: &Value, mounts: &mut Vec<String>) {
                if let Some(values) = node.get("mountpoints").and_then(Value::as_array) {
                    for value in values.iter().filter_map(Value::as_str) {
                        if !value.is_empty() && !mounts.iter().any(|mount| mount == value) {
                            mounts.push(value.to_string());
                        }
                    }
                }
                if let Some(children) = node.get("children").and_then(Value::as_array) {
                    for child in children {
                        collect_mountpoints(child, mounts);
                    }
                }
            }
            let mut mountpoints = Vec::new();
            collect_mountpoints(row, &mut mountpoints);
            Some(BlockDevice {
                path: stable_disk_path(path, transport),
                model: get("model").trim().to_string(),
                size: human_size(size),
                transport: if transport.is_empty() {
                    "removable".into()
                } else {
                    transport.into()
                },
                removable,
                mounted: !mountpoints.is_empty(),
                mountpoints,
            })
        })
        .collect()
}

fn stable_disk_path(path: &str, transport: &str) -> String {
    let Ok(target) = fs::canonicalize(path) else {
        return path.to_owned();
    };
    let Ok(entries) = fs::read_dir("/dev/disk/by-id") else {
        return path.to_owned();
    };
    let mut matches = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            !name.contains("-part") && (!name.starts_with("usb-") || transport == "usb")
        })
        .filter_map(|entry| {
            (fs::canonicalize(entry.path()).ok().as_ref() == Some(&target))
                .then(|| entry.path().to_string_lossy().into_owned())
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches
        .iter()
        .find(|candidate| candidate.contains("/by-id/usb-"))
        .cloned()
        .or_else(|| matches.first().cloned())
        .unwrap_or_else(|| path.to_owned())
}

fn human_size(bytes: u64) -> String {
    let gib = bytes as f64 / 1_073_741_824.0;
    if gib >= 1.0 {
        format!("{gib:.1} GiB")
    } else {
        format!("{:.0} MiB", bytes as f64 / 1_048_576.0)
    }
}

pub fn discover_firmware(root: &Path) -> FirmwareInfo {
    let code = ["OVMF_CODE_4M.fd", "OVMF_CODE.fd"]
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file());
    let vars = ["OVMF_VARS_4M.fd", "OVMF_VARS.fd"]
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file());
    FirmwareInfo { code, vars }
}

pub fn firmware_candidates(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("OVMF_"))
            && path.extension().is_some_and(|ext| ext == "fd")
        {
            candidates.push(path);
        }
    }
    candidates.sort();
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_iommu_flags() {
        assert!(iommu_flag_enabled("quiet intel_iommu=on iommu=pt"));
        assert!(iommu_flag_enabled("amd_iommu=on quiet"));
        assert!(!iommu_flag_enabled("quiet intel_iommu=off"));
        assert!(!iommu_flag_enabled("quiet iommu=pt"));
    }

    #[test]
    fn reads_iommu_groups_and_pci_metadata_from_sysfs_fixture() {
        let root = std::env::temp_dir().join(format!("winship-sysfs-{}", std::process::id()));
        let group = root.join("groups/7/devices");
        let pci = root.join("pci/0000:01:00.0");
        fs::create_dir_all(&group).unwrap();
        fs::create_dir_all(&pci).unwrap();
        fs::write(group.join("0000:01:00.0"), "").unwrap();
        fs::write(pci.join("vendor"), "0x10de\n").unwrap();
        fs::write(pci.join("device"), "0x2684\n").unwrap();
        fs::write(pci.join("class"), "0x030000\n").unwrap();

        let groups = discover_iommu_groups(&root.join("groups"));
        let devices = discover_pci_devices(&root.join("pci"));
        assert_eq!(groups, vec![(7, vec!["0000:01:00.0".into()])]);
        assert_eq!(devices.len(), 1);
        assert!(devices[0].is_nvidia());
        assert_eq!(devices[0].iommu_group, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_debian_ovmf_firmware_names() {
        let root = std::env::temp_dir().join(format!("winship-ovmf-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("OVMF_CODE_4M.fd"), "").unwrap();
        fs::write(root.join("OVMF_VARS_4M.fd"), "").unwrap();
        let firmware = discover_firmware(&root);
        assert_eq!(firmware.code, Some(root.join("OVMF_CODE_4M.fd")));
        assert_eq!(firmware.vars, Some(root.join("OVMF_VARS_4M.fd")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_uefi_secure_boot_variable() {
        let root = std::env::temp_dir().join(format!("winship-efivar-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("SecureBoot-test"), [0, 0, 0, 0, 1]).unwrap();
        assert_eq!(detect_secure_boot(&root), Some(true));
        fs::write(root.join("SecureBoot-test"), [0, 0, 0, 0, 0]).unwrap();
        assert_eq!(detect_secure_boot(&root), Some(false));
        fs::remove_dir_all(root).unwrap();
    }
}
