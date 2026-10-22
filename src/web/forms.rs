use crate::model::{HostSnapshot, NetworkMode, VmConfig, parse_pci_address};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct NoticeParams {
    pub notice: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct FormData {
    pairs: Vec<(String, String)>,
}

impl FormData {
    pub fn parse(body: &[u8]) -> Result<Self, String> {
        let body = std::str::from_utf8(body).map_err(|_| "表单不是有效的 UTF-8".to_string())?;
        if body.len() > 256 * 1024 {
            return Err("表单内容过大".into());
        }
        let mut pairs = Vec::new();
        for part in body.split('&') {
            if part.is_empty() {
                continue;
            }
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            pairs.push((percent_decode(key)?, percent_decode(value)?));
        }
        Ok(Self { pairs })
    }

    pub fn first(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value.as_str()))
    }

    pub fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs
            .iter()
            .filter_map(move |(key, value)| (key == name).then_some(value.as_str()))
    }

    pub fn checked(&self, name: &str) -> bool {
        self.values(name).any(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "on" | "true" | "yes"
            )
        })
    }

    pub fn required(&self, name: &str, label: &str) -> Result<String, String> {
        self.first(name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("{label}不能为空"))
    }
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' => {
                let Some(high) = bytes.get(index + 1).and_then(|byte| hex(*byte)) else {
                    return Err("表单含有无效的百分号转义".into());
                };
                let Some(low) = bytes.get(index + 2).and_then(|byte| hex(*byte)) else {
                    return Err("表单含有无效的百分号转义".into());
                };
                output.push((high << 4) | low);
                index += 3;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| "表单不是有效的 UTF-8".into())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn url_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

pub fn redirect_notice(path: &str, message: impl AsRef<str>, is_error: bool) -> Response {
    let separator = if path.contains('?') { '&' } else { '?' };
    let error = if is_error { "1" } else { "0" };
    Redirect::to(&format!(
        "{path}{separator}notice={}&error={error}",
        url_encode(message.as_ref())
    ))
    .into_response()
}

pub fn parse_config(
    form: &FormData,
    host: &HostSnapshot,
    current: &VmConfig,
) -> Result<VmConfig, String> {
    let number = |name: &str, label: &str| -> Result<u16, String> {
        form.required(name, label)?
            .parse::<u16>()
            .map_err(|_| format!("{label}必须是整数"))
    };
    let number_u32 = |name: &str, label: &str| -> Result<u32, String> {
        form.required(name, label)?
            .parse::<u32>()
            .map_err(|_| format!("{label}必须是整数"))
    };

    let mut pci_devices = Vec::new();
    for address in form.values("pci") {
        if parse_pci_address(address).is_none() {
            return Err(format!("无效的 PCI 地址：{address}"));
        }
        if !host
            .pci_devices
            .iter()
            .any(|device| device.address == address)
        {
            return Err(format!("PCI 设备不存在：{address}"));
        }
        if !pci_devices.iter().any(|selected| selected == address) {
            pci_devices.push(address.to_string());
        }
    }

    let mut usb_devices = Vec::new();
    for value in form.values("usb") {
        let index = value
            .parse::<usize>()
            .map_err(|_| "无效的 USB 设备选择".to_string())?;
        let device = host
            .usb_devices
            .get(index)
            .ok_or_else(|| "USB 设备列表已变化，请刷新页面后重试".to_string())?;
        let selection = device.selection();
        if !usb_devices.contains(&selection) {
            usb_devices.push(selection);
        }
    }

    let mut physical_disks = Vec::new();
    for value in form.values("physical_disk") {
        let index = value
            .parse::<usize>()
            .map_err(|_| "无效的物理磁盘选择".to_string())?;
        let device = host
            .block_devices
            .get(index)
            .ok_or_else(|| "物理磁盘列表已变化，请刷新页面后重试".to_string())?;
        if !physical_disks.contains(&device.path) {
            physical_disks.push(device.path.clone());
        }
    }

    let network_mode = match form.first("network_mode").unwrap_or("nat") {
        "nat" => NetworkMode::Nat,
        "bridge" => NetworkMode::Bridge,
        "macvtap" => NetworkMode::Macvtap,
        _ => return Err("无效的网络模式".into()),
    };

    if network_mode == NetworkMode::Bridge {
        let interface = host
            .network_interfaces
            .iter()
            .find(|interface| interface.name == form.first("network_interface").unwrap_or_default())
            .ok_or_else(|| "桥接模式需要选择一个有线接口".to_string())?;
        if interface.kind != "ethernet" {
            return Err("桥接模式只支持有线接口。".into());
        }
    }

    let config = VmConfig {
        vm_uuid: current.vm_uuid.clone(),
        windows_iso: form
            .first("windows_iso")
            .unwrap_or_default()
            .trim()
            .to_string(),
        virtio_iso: form
            .first("virtio_iso")
            .unwrap_or_default()
            .trim()
            .to_string(),
        disk_path: form
            .first("disk_path")
            .unwrap_or_default()
            .trim()
            .to_string(),
        disk_size_gib: number("disk_size_gib", "磁盘容量")?,
        cpu_cores: number("cpu_cores", "CPU 核心数")?,
        startup_memory_mib: number_u32("startup_memory_mib", "启动内存")?,
        maximum_memory_mib: number_u32("maximum_memory_mib", "最大内存")?,
        pci_devices,
        usb_devices,
        physical_disks,
        network_mode,
        network_interface: form
            .first("network_interface")
            .unwrap_or_default()
            .trim()
            .to_string(),
        boot_from_cd: form.checked("boot_from_cd"),
    };
    config.validate().map_err(|error| error.to_string())?;
    Ok(config)
}

pub fn save_config(config: &VmConfig) -> Result<(), String> {
    crate::config::save(config).map_err(|error| format!("无法保存配置：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repeated_checkbox_fields() {
        let form =
            FormData::parse(b"pci=0000%3A01%3A00.0&pci=0000%3A01%3A00.1&disk=hello+world").unwrap();
        assert_eq!(
            form.values("pci").collect::<Vec<_>>(),
            vec!["0000:01:00.0", "0000:01:00.1"]
        );
        assert_eq!(form.first("disk"), Some("hello world"));
    }

    #[test]
    fn encodes_notice_for_redirects() {
        assert_eq!(
            url_encode("失败 / 检查"),
            "%E5%A4%B1%E8%B4%A5%20%2F%20%E6%A3%80%E6%9F%A5"
        );
    }
}
