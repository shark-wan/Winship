use crate::{
    config,
    libvirt::{self, PowerState, VmStatus},
    model::{BlockDevice, HostSnapshot, PciDevice, UsbDevice, VmConfig},
    privileged,
};
use askama::Template;
use std::path::Path;

use super::forms::NoticeParams;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Default)]
pub struct Chrome {
    pub version: &'static str,
    pub active: &'static str,
    pub notice_visible: bool,
    pub notice_message: String,
    pub notice_error: bool,
}

impl Chrome {
    pub fn new(active: &'static str, params: &NoticeParams) -> Self {
        let message = params.notice.clone().unwrap_or_default();
        Self {
            version: VERSION,
            active,
            notice_visible: !message.is_empty(),
            notice_message: message,
            notice_error: params.error.as_deref() == Some("1"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VmSummary {
    pub label: String,
    pub state_class: &'static str,
    pub active: bool,
    pub defined: bool,
    pub error: String,
    pub has_error: bool,
    pub cpu_count: String,
    pub cpu_percent: String,
    pub memory_current: String,
    pub memory_max: String,
}

impl VmSummary {
    pub fn from_status(status: &VmStatus, cpu_percent: Option<f32>) -> Self {
        let (label, state_class) = match &status.state {
            PowerState::NotDefined => ("未定义".to_string(), "off"),
            PowerState::ShutOff => ("已停止".to_string(), "warn"),
            PowerState::Running => ("运行中".to_string(), "ok"),
            PowerState::Paused => ("已暂停".to_string(), "warn"),
            PowerState::Other(value) => (value.clone(), "warn"),
        };
        Self {
            active: status.state.is_active(),
            defined: !matches!(status.state, PowerState::NotDefined),
            label,
            state_class,
            error: String::new(),
            has_error: false,
            cpu_count: status
                .cpu_count
                .map(|value| value.to_string())
                .unwrap_or_else(|| "—".into()),
            cpu_percent: percent(cpu_percent),
            memory_current: kib_gib(status.used_memory_kib),
            memory_max: kib_gib(status.max_memory_kib),
        }
    }

    pub fn unavailable(error: impl Into<String>) -> Self {
        Self {
            label: "无法读取".into(),
            state_class: "danger",
            active: false,
            defined: false,
            error: error.into(),
            has_error: true,
            cpu_count: "—".into(),
            cpu_percent: "—".into(),
            memory_current: "—".into(),
            memory_max: "—".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadinessItem {
    pub title: String,
    pub detail: String,
    pub level: &'static str,
}

#[derive(Debug, Clone)]
pub struct PciRow {
    pub address: String,
    pub vendor_id: String,
    pub device_id: String,
    pub class_label: String,
    pub description: String,
    pub driver: String,
    pub group_label: String,
    pub group_members: String,
    pub group_complete: bool,
    pub selected: bool,
    pub vfio_ready: bool,
    pub is_nvidia: bool,
    pub is_audio: bool,
    pub is_usb_controller: bool,
    pub recommended: bool,
}

#[derive(Debug, Clone)]
pub struct UsbRow {
    pub index: usize,
    pub name: String,
    pub vendor_id: String,
    pub product_id: String,
    pub address: String,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct DiskRow {
    pub index: usize,
    pub path: String,
    pub model: String,
    pub size: String,
    pub transport: String,
    pub mounted: bool,
    pub mountpoint: String,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct NetworkRow {
    pub name: String,
    pub kind: String,
    pub state: String,
    pub addresses: String,
    pub wired: bool,
}

#[derive(Debug, Clone)]
pub struct InterfaceOption {
    pub value: String,
    pub label: String,
    pub wired: bool,
}

#[derive(Debug, Clone)]
pub struct VfioSummary {
    pub selected_ids: Vec<String>,
    pub applied_ids: Vec<String>,
    pub conflicts: Vec<String>,
    pub all_bound: bool,
    pub all_applied: bool,
    pub reboot_required: bool,
    pub ready: bool,
}

pub struct PageData {
    pub chrome: Chrome,
    pub host: HostSnapshot,
    pub config: VmConfig,
    pub status: VmStatus,
    pub status_error: Option<String>,
    pub cpu_percent: Option<f32>,
    pub vfio: VfioSummary,
    pub pci_rows: Vec<PciRow>,
    pub usb_rows: Vec<UsbRow>,
    pub disk_rows: Vec<DiskRow>,
    pub network_rows: Vec<NetworkRow>,
    pub interface_options: Vec<InterfaceOption>,
    pub readiness: Vec<ReadinessItem>,
}

impl PageData {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        active: &'static str,
        params: &NoticeParams,
        host: HostSnapshot,
        config: VmConfig,
        status: Result<VmStatus, libvirt::LibvirtError>,
        cpu_percent: Option<f32>,
    ) -> Self {
        let (status, status_error) = match status {
            Ok(status) => (status, None),
            Err(error) => (VmStatus::default(), Some(error.to_string())),
        };
        let vfio = vfio_summary(&host, &config);
        let pci_rows = pci_rows(&host, &config);
        let usb_rows = usb_rows(&host, &config);
        let disk_rows = disk_rows(&host, &config);
        let network_rows = network_rows(&host);
        let interface_options = host
            .network_interfaces
            .iter()
            .map(|interface| InterfaceOption {
                value: interface.name.clone(),
                label: format!(
                    "{} · {}{}",
                    interface.name,
                    interface.kind,
                    if interface.addresses.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", interface.addresses.join(", "))
                    }
                ),
                wired: interface.kind == "ethernet",
            })
            .collect();
        let readiness = readiness(&host, &config, &status, &vfio);
        Self {
            chrome: Chrome::new(active, params),
            host,
            config,
            status,
            status_error,
            cpu_percent,
            vfio,
            pci_rows,
            usb_rows,
            disk_rows,
            network_rows,
            interface_options,
            readiness,
        }
    }

    pub fn vm(&self) -> VmSummary {
        if let Some(error) = &self.status_error {
            VmSummary::unavailable(error.clone())
        } else {
            VmSummary::from_status(&self.status, self.cpu_percent)
        }
    }
}

#[derive(Template)]
#[template(path = "overview.html")]
pub struct OverviewView {
    pub chrome: Chrome,
    pub vm: VmSummary,
    pub host: HostSnapshot,
    pub config: VmConfig,
    pub readiness: Vec<ReadinessItem>,
    pub vfio: VfioSummary,
    pub cpu_percent: String,
    pub host_memory_used: String,
    pub selected_gpu: String,
    pub selected_usb_controllers: usize,
    pub selected_usb_devices: usize,
    pub selected_disks: usize,
    pub disk_status: String,
}

impl OverviewView {
    pub fn new(data: PageData) -> Self {
        let selected_gpu = data
            .pci_rows
            .iter()
            .filter(|row| row.selected && (row.is_nvidia || row.is_audio))
            .map(|row| row.description.clone())
            .collect::<Vec<_>>()
            .join(" + ");
        let disk_status = format!(
            "{}{}",
            data.config.disk_path,
            if Path::new(&data.config.disk_path).is_file() {
                format!(" · {}", file_size(&data.config.disk_path))
            } else {
                " · 尚未创建".into()
            }
        );
        let vm = data.vm();
        let host_memory_used = percent(Some(data.host.memory_used_percent()));
        let selected_usb_controllers = data
            .pci_rows
            .iter()
            .filter(|row| row.selected && row.is_usb_controller)
            .count();
        let selected_usb_devices = data.usb_rows.iter().filter(|row| row.selected).count();
        let selected_disks = data.disk_rows.iter().filter(|row| row.selected).count();
        Self {
            chrome: data.chrome,
            vm,
            host: data.host,
            config: data.config,
            readiness: data.readiness,
            vfio: data.vfio,
            cpu_percent: percent(data.cpu_percent),
            host_memory_used,
            selected_gpu: if selected_gpu.is_empty() {
                "未选择 NVIDIA 显卡".into()
            } else {
                selected_gpu
            },
            selected_usb_controllers,
            selected_usb_devices,
            selected_disks,
            disk_status,
        }
    }
}

#[derive(Template)]
#[template(path = "host.html")]
pub struct HostView {
    pub chrome: Chrome,
    pub vm: VmSummary,
    pub host: HostSnapshot,
    pub vfio: VfioSummary,
    pub pci_rows: Vec<PciRow>,
    pub usb_rows: Vec<UsbRow>,
    pub disk_rows: Vec<DiskRow>,
    pub network_rows: Vec<NetworkRow>,
    pub iommu_groups: usize,
    pub iommu_ready: bool,
    pub iommu_hint: &'static str,
    pub secure_boot: String,
    pub secure_boot_class: &'static str,
    pub firmware_ready: bool,
    pub ovmf_code: String,
    pub ovmf_vars: String,
    pub memory_used: String,
    pub balloon_current: String,
    pub balloon_max: String,
}

impl HostView {
    pub fn new(data: PageData) -> Self {
        Self {
            vm: data.vm(),
            vfio: data.vfio,
            iommu_groups: data.host.iommu_groups.len(),
            iommu_ready: data.host.iommu_enabled(),
            iommu_hint: data.host.iommu_hint(),
            secure_boot: match data.host.secure_boot_enabled {
                Some(true) => "已启用（不合格）".into(),
                Some(false) => "已关闭".into(),
                None => "无法读取".into(),
            },
            secure_boot_class: match data.host.secure_boot_enabled {
                Some(false) => "ok",
                Some(true) => "error",
                None => "warn",
            },
            firmware_ready: data.host.firmware.code.is_some() && data.host.firmware.vars.is_some(),
            ovmf_code: data
                .host
                .firmware
                .code
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "未找到".into()),
            ovmf_vars: data
                .host
                .firmware
                .vars
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "未找到".into()),
            memory_used: percent(Some(data.host.memory_used_percent())),
            balloon_current: kib_gib(data.status.used_memory_kib),
            balloon_max: kib_gib(data.status.max_memory_kib),
            chrome: data.chrome,
            host: data.host,
            pci_rows: data.pci_rows,
            usb_rows: data.usb_rows,
            disk_rows: data.disk_rows,
            network_rows: data.network_rows,
        }
    }
}

#[derive(Template)]
#[template(path = "vm.html")]
pub struct VmView {
    pub chrome: Chrome,
    pub vm: VmSummary,
    pub config: VmConfig,
    pub pci_rows: Vec<PciRow>,
    pub usb_rows: Vec<UsbRow>,
    pub disk_rows: Vec<DiskRow>,
    pub network_rows: Vec<NetworkRow>,
    pub interface_options: Vec<InterfaceOption>,
    pub windows_exists: bool,
    pub windows_readable: bool,
    pub virtio_exists: bool,
    pub virtio_readable: bool,
    pub disk_exists: bool,
    pub disk_size: String,
    pub iommu_ready: bool,
    pub iommu_hint: &'static str,
    pub firmware_ready: bool,
    pub media_dir: String,
    pub image_root: &'static str,
    pub default_disk_path: String,
    pub network_mode: &'static str,
    pub selected_usb_controllers: usize,
    pub startup_memory: String,
    pub maximum_memory: String,
}

impl VmView {
    pub fn new(data: PageData) -> Self {
        let windows_exists = Path::new(&data.config.windows_iso).is_file();
        let virtio_exists = Path::new(&data.config.virtio_iso).is_file();
        let disk_exists = Path::new(&data.config.disk_path).is_file();
        let selected_usb_controllers = data
            .pci_rows
            .iter()
            .filter(|row| row.selected && row.is_usb_controller)
            .count();
        let network_mode = match data.config.network_mode {
            crate::model::NetworkMode::Nat => "nat",
            crate::model::NetworkMode::Bridge => "bridge",
            crate::model::NetworkMode::Macvtap => "macvtap",
        };
        Self {
            vm: data.vm(),
            windows_exists,
            windows_readable: windows_exists && config::qemu_can_read(&data.config.windows_iso),
            virtio_exists,
            virtio_readable: virtio_exists && config::qemu_can_read(&data.config.virtio_iso),
            disk_exists,
            disk_size: if disk_exists {
                file_size(&data.config.disk_path)
            } else {
                "未创建".into()
            },
            iommu_ready: data.host.iommu_enabled(),
            iommu_hint: data.host.iommu_hint(),
            firmware_ready: data.host.firmware.code.is_some() && data.host.firmware.vars.is_some(),
            media_dir: config::media_dir().to_string_lossy().into_owned(),
            image_root: config::IMAGE_ROOT,
            default_disk_path: config::default_disk_path(),
            network_mode,
            selected_usb_controllers,
            startup_memory: gib_from_mib(data.config.startup_memory_mib),
            maximum_memory: gib_from_mib(data.config.maximum_memory_mib),
            chrome: data.chrome,
            config: data.config,
            pci_rows: data.pci_rows,
            usb_rows: data.usb_rows,
            disk_rows: data.disk_rows,
            network_rows: data.network_rows,
            interface_options: data.interface_options,
        }
    }
}

#[derive(Template)]
#[template(path = "about.html")]
pub struct AboutView {
    pub chrome: Chrome,
    pub host: HostSnapshot,
    pub vm: VmSummary,
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginView {
    pub error_visible: bool,
    pub error_message: String,
    pub version: &'static str,
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorView {
    pub status: u16,
    pub title: String,
    pub message: String,
    pub version: &'static str,
}

fn vfio_summary(host: &HostSnapshot, config: &VmConfig) -> VfioSummary {
    let selected_ids = host.selected_vfio_ids(config).unwrap_or_default();
    let conflicts = host.selected_vfio_conflicts(config).unwrap_or_default();
    let conf_text = std::fs::read_to_string("/etc/modprobe.d/vfio.conf").unwrap_or_default();
    let applied_ids = privileged::vfio_conf_ids(&conf_text);
    let all_bound = !config.pci_devices.is_empty()
        && config
            .pci_devices
            .iter()
            .all(|address| libvirt::pci_driver(address).as_deref() == Some("vfio-pci"));
    let all_applied =
        !selected_ids.is_empty() && selected_ids.iter().all(|id| applied_ids.contains(id));
    VfioSummary {
        reboot_required: all_applied && !all_bound,
        ready: !config.pci_devices.is_empty() && all_bound,
        selected_ids,
        applied_ids,
        conflicts,
        all_bound,
        all_applied,
    }
}

fn pci_rows(host: &HostSnapshot, config: &VmConfig) -> Vec<PciRow> {
    host.pci_devices
        .iter()
        .map(|device| pci_row(host, config, device))
        .collect()
}

fn pci_row(host: &HostSnapshot, config: &VmConfig, device: &PciDevice) -> PciRow {
    let members = host.group_members(device.iommu_group);
    PciRow {
        address: device.address.clone(),
        vendor_id: device.vendor_id.clone(),
        device_id: device.device_id.clone(),
        class_label: format!("{:06x}", device.class),
        description: device.description.clone(),
        driver: device.driver.clone().unwrap_or_else(|| "未绑定".into()),
        group_label: device
            .iommu_group
            .map(|group| format!("IOMMU {group}"))
            .unwrap_or_else(|| "无分组".into()),
        group_members: members.join(", "),
        group_complete: host.selected_group_is_complete(config, device.iommu_group),
        selected: config.pci_devices.contains(&device.address),
        vfio_ready: device.driver.as_deref() == Some("vfio-pci"),
        is_nvidia: device.is_nvidia(),
        is_audio: device.is_audio(),
        is_usb_controller: device.is_usb_controller(),
        recommended: (device.is_nvidia() && (device.is_display() || device.is_audio()))
            || device.is_usb_controller(),
    }
}

fn usb_rows(host: &HostSnapshot, config: &VmConfig) -> Vec<UsbRow> {
    host.usb_devices
        .iter()
        .enumerate()
        .map(|(index, device)| usb_row(index, device, config))
        .collect()
}

fn usb_row(index: usize, device: &UsbDevice, config: &VmConfig) -> UsbRow {
    UsbRow {
        index,
        name: if device.product.is_empty() {
            "USB 设备".into()
        } else {
            device.product.clone()
        },
        vendor_id: device.vendor_id.clone(),
        product_id: device.product_id.clone(),
        address: format!("总线 {} · 地址 {}", device.bus, device.address),
        selected: config.usb_devices.contains(&device.selection()),
    }
}

fn disk_rows(host: &HostSnapshot, config: &VmConfig) -> Vec<DiskRow> {
    host.block_devices
        .iter()
        .enumerate()
        .map(|(index, device)| disk_row(index, device, config))
        .collect()
}

fn disk_row(index: usize, device: &BlockDevice, config: &VmConfig) -> DiskRow {
    DiskRow {
        index,
        path: device.path.clone(),
        model: if device.model.is_empty() {
            "未命名磁盘".into()
        } else {
            device.model.clone()
        },
        size: device.size.clone(),
        transport: device.transport.clone(),
        mounted: device.mounted,
        mountpoint: device.mountpoints.join(", "),
        selected: config.physical_disks.contains(&device.path),
    }
}

fn network_rows(host: &HostSnapshot) -> Vec<NetworkRow> {
    host.network_interfaces
        .iter()
        .map(|interface| NetworkRow {
            name: interface.name.clone(),
            kind: interface.kind.clone(),
            state: interface.state.clone(),
            addresses: interface.addresses.join(", "),
            wired: interface.kind == "ethernet",
        })
        .collect()
}

fn readiness(
    host: &HostSnapshot,
    config: &VmConfig,
    status: &VmStatus,
    vfio: &VfioSummary,
) -> Vec<ReadinessItem> {
    let mut items = Vec::new();
    items.push(if host.iommu_enabled() {
        ReadinessItem {
            title: "IOMMU".into(),
            detail: format!("内核已创建 {} 个 IOMMU group。", host.iommu_groups.len()),
            level: "ok",
        }
    } else {
        ReadinessItem {
            title: "IOMMU".into(),
            detail: format!(
                "未检测到 IOMMU group。请在 GRUB 加入 {} 后重启。",
                host.iommu_hint()
            ),
            level: "error",
        }
    });
    items.push(match host.secure_boot_enabled {
        Some(true) => ReadinessItem {
            title: "Secure Boot".into(),
            detail: "Secure Boot 已启用，请先在固件设置中关闭。".into(),
            level: "error",
        },
        Some(false) => ReadinessItem {
            title: "Secure Boot".into(),
            detail: "Secure Boot 已关闭。".into(),
            level: "ok",
        },
        None => ReadinessItem {
            title: "Secure Boot".into(),
            detail: "无法读取 EFI SecureBoot 变量；请确认当前为 UEFI 启动。".into(),
            level: "warn",
        },
    });
    items.push(
        if host.firmware.code.is_some() && host.firmware.vars.is_some() {
            ReadinessItem {
                title: "OVMF 固件".into(),
                detail: "Q35 + OVMF 所需固件文件已就绪。".into(),
                level: "ok",
            }
        } else {
            ReadinessItem {
                title: "OVMF 固件".into(),
                detail: "缺少 OVMF_CODE / OVMF_VARS，请安装 ovmf 包。".into(),
                level: "error",
            }
        },
    );
    if config.pci_devices.is_empty() {
        items.push(ReadinessItem {
            title: "PCI 直通".into(),
            detail: "尚未选择 NVIDIA 显卡、音频功能或 USB 控制器。".into(),
            level: "warn",
        });
    } else if !vfio.conflicts.is_empty() {
        items.push(ReadinessItem {
            title: "PCI 直通".into(),
            detail: format!(
                "选中的 vendor:device ID 也会匹配未选设备：{}",
                vfio.conflicts.join(", ")
            ),
            level: "error",
        });
    } else {
        let incomplete = config
            .pci_devices
            .iter()
            .filter_map(|address| {
                let device = host
                    .pci_devices
                    .iter()
                    .find(|device| device.address == *address)?;
                (!host.selected_group_is_complete(config, device.iommu_group)).then_some(device)
            })
            .map(|device| {
                device
                    .iommu_group
                    .map_or("unknown".to_string(), |group| group.to_string())
            })
            .collect::<Vec<_>>();
        items.push(if incomplete.is_empty() {
            ReadinessItem {
                title: "PCI 直通".into(),
                detail: "已选设备覆盖完整 IOMMU group。".into(),
                level: "ok",
            }
        } else {
            ReadinessItem {
                title: "PCI 直通".into(),
                detail: format!("IOMMU group {} 未完整选择。", incomplete.join(", ")),
                level: "error",
            }
        });
    }
    items.push(if vfio.selected_ids.is_empty() {
        ReadinessItem {
            title: "VFIO 绑定".into(),
            detail: "没有选择需要 VFIO 的 PCI 设备。".into(),
            level: "info",
        }
    } else if !vfio.all_applied {
        ReadinessItem {
            title: "VFIO 绑定".into(),
            detail: "配置尚未写入。应用 VFIO 配置后需要重启主机。".into(),
            level: "warn",
        }
    } else if !vfio.all_bound {
        ReadinessItem {
            title: "VFIO 绑定".into(),
            detail: "配置已写入，但设备尚未绑定 vfio-pci。请重启主机。".into(),
            level: "warn",
        }
    } else {
        ReadinessItem {
            title: "VFIO 绑定".into(),
            detail: "所有选中的 PCI 设备已由 vfio-pci 接管。".into(),
            level: "ok",
        }
    });
    let windows_ok = Path::new(&config.windows_iso).is_file();
    let virtio_ok = Path::new(&config.virtio_iso).is_file();
    let windows_readable = config::qemu_can_read(&config.windows_iso);
    let virtio_readable = config::qemu_can_read(&config.virtio_iso);
    items.push(
        if windows_ok && virtio_ok && windows_readable && virtio_readable {
            ReadinessItem {
                title: "安装介质".into(),
                detail: "Windows ISO 与 VirtIO 驱动 ISO 均可由 QEMU 读取。".into(),
                level: "ok",
            }
        } else {
            ReadinessItem {
                title: "安装介质".into(),
                detail: if !windows_ok || !virtio_ok {
                    "请选择 Windows ISO 和 virtio-win.iso。".into()
                } else {
                    "ISO 位于 QEMU 无法访问的位置，请导入到媒体库。".into()
                },
                level: "warn",
            }
        },
    );
    items.push(if Path::new(&config.disk_path).is_file() {
        ReadinessItem {
            title: "虚拟磁盘".into(),
            detail: config.disk_path.clone(),
            level: "ok",
        }
    } else {
        ReadinessItem {
            title: "虚拟磁盘".into(),
            detail: "虚拟磁盘尚未创建。".into(),
            level: "warn",
        }
    });
    if !config.physical_disks.is_empty() {
        let mounted = host
            .block_devices
            .iter()
            .any(|disk| disk.mounted && config.physical_disks.contains(&disk.path));
        items.push(ReadinessItem {
            title: "物理磁盘".into(),
            detail: if mounted {
                "选中的物理磁盘仍挂载在宿主机上，请先卸载。".into()
            } else {
                "整盘直通将在虚拟机运行时独占该磁盘。".into()
            },
            level: if mounted { "error" } else { "ok" },
        });
    }
    if status.state.is_active() {
        items.push(ReadinessItem {
            title: "虚拟机运行中".into(),
            detail: "修改硬件配置前，请先在 Windows 中正常关机。".into(),
            level: "warn",
        });
    }
    items.push(ReadinessItem {
        title: "VirtIO Balloon".into(),
        detail: "Windows 安装完成后，请在 VirtIO ISO 中安装 Balloon 驱动，宿主机才能回收闲置内存。"
            .into(),
        level: "info",
    });
    items
}

pub fn percent(value: Option<f32>) -> String {
    value
        .map(|value| format!("{:.0}%", value.clamp(0.0, 100.0)))
        .unwrap_or_else(|| "—".into())
}

pub fn kib_gib(value: Option<u64>) -> String {
    value
        .map(|kib| format!("{:.1} GiB", kib as f64 / 1_048_576.0))
        .unwrap_or_else(|| "—".into())
}

fn gib_from_mib(value: u32) -> String {
    format!("{:.1} GiB", value as f64 / 1024.0)
}

fn file_size(path: &str) -> String {
    std::fs::metadata(path)
        .map(|metadata| {
            let bytes = metadata.len() as f64;
            if bytes >= 1_073_741_824.0 {
                format!("{:.1} GiB", bytes / 1_073_741_824.0)
            } else {
                format!("{:.0} MiB", bytes / 1_048_576.0)
            }
        })
        .unwrap_or_else(|_| "—".into())
}
