use crate::host;
use crate::libvirt::{self, PowerState, VmStatus};
use crate::model::{BlockDevice, HostSnapshot, PciDevice, UsbDevice, VM_NAME, VmConfig};
use crate::privileged;
use eframe::egui::{self, Align, Align2, Color32, FontFamily, FontId, Layout, RichText, Stroke};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const BACKGROUND: Color32 = Color32::from_rgb(244, 246, 245);
const PANEL: Color32 = Color32::from_rgb(255, 255, 255);
const NAV: Color32 = Color32::from_rgb(27, 37, 41);
const TEXT: Color32 = Color32::from_rgb(31, 43, 46);
const MUTED: Color32 = Color32::from_rgb(108, 119, 121);
const GREEN: Color32 = Color32::from_rgb(38, 132, 91);
const BLUE: Color32 = Color32::from_rgb(48, 112, 145);
const AMBER: Color32 = Color32::from_rgb(179, 100, 34);
const RED: Color32 = Color32::from_rgb(171, 54, 48);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Host,
    VirtualMachine,
}

struct Notice {
    message: String,
    is_error: bool,
    created: Instant,
}

pub struct WinshipApp {
    page: Page,
    config: VmConfig,
    host: HostSnapshot,
    status: VmStatus,
    status_error: Option<String>,
    cpu_usage_percent: Option<f32>,
    previous_cpu_sample: Option<(Instant, u64)>,
    last_refresh: Instant,
    force_stop_confirmation: bool,
    notice: Option<Notice>,
    admin_task: Option<Receiver<Result<String, String>>>,
    admin_busy: bool,
}

impl WinshipApp {
    pub fn new(context: &eframe::CreationContext<'_>) -> Self {
        configure_theme(&context.egui_ctx);
        let host = host::inspect_host();
        let (status, status_error) = match libvirt::domain_status() {
            Ok(status) => (status, None),
            Err(error) => (VmStatus::default(), Some(error.to_string())),
        };
        Self {
            page: Page::Overview,
            config: load_config(),
            host,
            status,
            status_error,
            cpu_usage_percent: None,
            previous_cpu_sample: None,
            last_refresh: Instant::now(),
            force_stop_confirmation: false,
            notice: None,
            admin_task: None,
            admin_busy: false,
        }
    }

    fn refresh(&mut self) {
        self.host = host::inspect_host();
        match libvirt::domain_status() {
            Ok(status) => {
                if let Some(cpu_time_ns) = status.cpu_time_ns {
                    if let Some((sampled_at, previous_cpu_time)) = self.previous_cpu_sample {
                        let elapsed_ns = sampled_at.elapsed().as_nanos().max(1) as f64;
                        let cpu_count = status.cpu_count.unwrap_or(1).max(1) as f64;
                        let delta_ns = cpu_time_ns.saturating_sub(previous_cpu_time) as f64;
                        self.cpu_usage_percent = Some(
                            ((delta_ns / elapsed_ns) * 100.0 / cpu_count).clamp(0.0, 100.0) as f32,
                        );
                    }
                    self.previous_cpu_sample = Some((Instant::now(), cpu_time_ns));
                } else {
                    self.cpu_usage_percent = None;
                    self.previous_cpu_sample = None;
                }
                self.status = status;
                self.status_error = None;
            }
            Err(error) => self.status_error = Some(error.to_string()),
        }
        self.last_refresh = Instant::now();
    }

    fn set_notice(&mut self, message: impl Into<String>, is_error: bool) {
        self.notice = Some(Notice {
            message: message.into(),
            is_error,
            created: Instant::now(),
        });
    }

    fn poll_admin_task(&mut self) {
        let Some(receiver) = self.admin_task.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.admin_task = None;
                self.admin_busy = false;
                match result {
                    Ok(message) => {
                        self.set_notice(
                            if message.is_empty() {
                                String::from("VFIO 配置已写入；请重启主机使设备绑定生效。")
                            } else {
                                message
                            },
                            false,
                        );
                        self.refresh();
                    }
                    Err(error) => self.set_notice(error, true),
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.admin_task = None;
                self.admin_busy = false;
                self.set_notice("特权操作意外退出。", true);
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn start_apply_vfio(&mut self) {
        if self.host.secure_boot_enabled == Some(true) {
            self.set_notice(
                "Secure Boot is enabled. Disable it in firmware settings before applying VFIO.",
                true,
            );
            return;
        }
        let ids = match self.host.selected_vfio_ids(&self.config) {
            Ok(ids) if !ids.is_empty() => ids,
            Ok(_) => {
                self.set_notice("请先在虚拟机页面选择要直通的 PCI 设备。", true);
                return;
            }
            Err(error) => {
                self.set_notice(error.to_string(), true);
                return;
            }
        };
        let selected = self.config.pci_devices.clone();
        match self.host.selected_vfio_conflicts(&self.config) {
            Ok(conflicts) if !conflicts.is_empty() => {
                self.set_notice(
                    format!(
                        "vfio-pci IDs would also bind unselected devices: {}. Select them for the VM or change the device set before applying VFIO.",
                        conflicts.join(", ")
                    ),
                    true,
                );
                return;
            }
            Err(error) => {
                self.set_notice(error.to_string(), true);
                return;
            }
            _ => {}
        }
        let incomplete = selected.iter().find_map(|address| {
            let device = self
                .host
                .pci_devices
                .iter()
                .find(|device| device.address == *address)?;
            (!self
                .host
                .selected_group_is_complete(&self.config, device.iommu_group))
            .then_some(device.iommu_group)
        });
        if let Some(group) = incomplete {
            self.set_notice(
                format!(
                    "IOMMU group {} is incomplete. Select every member before applying VFIO.",
                    group
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "unknown".into())
                ),
                true,
            );
            return;
        }

        let (sender, receiver) = mpsc::channel();
        self.admin_task = Some(receiver);
        self.admin_busy = true;
        std::thread::spawn(move || {
            let result = privileged::request_vfio_apply(&ids).map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
    }

    fn define_vm(&mut self) {
        if self.status.state.is_active() {
            self.set_notice(
                "Shut down the VM before saving its hardware configuration.",
                true,
            );
            return;
        }
        let updating = !matches!(self.status.state, PowerState::NotDefined);
        let validation = if updating {
            self.config.validate()
        } else {
            self.config.validate_for_create()
        };
        if let Err(error) = validation {
            self.set_notice(error.to_string(), true);
            return;
        }
        if let Err(error) = libvirt::validate_install_media(&self.config) {
            self.set_notice(error.to_string(), true);
            return;
        }
        if let Err(error) = libvirt::validate_physical_disks(&self.config) {
            self.set_notice(error.to_string(), true);
            return;
        }
        if let Some(group) = self.config.pci_devices.iter().find_map(|address| {
            let device = self
                .host
                .pci_devices
                .iter()
                .find(|device| device.address == *address)?;
            (!self
                .host
                .selected_group_is_complete(&self.config, device.iommu_group))
            .then_some(device.iommu_group)
        }) {
            self.set_notice(format!("IOMMU group {} is incomplete. Select every group member before defining the VM.", group.map(|id| id.to_string()).unwrap_or_else(|| "unknown".into())), true);
            return;
        }
        let firmware = self.host.firmware.clone();
        let xml = match libvirt::generate_domain_xml(&self.config, &firmware) {
            Ok(xml) => xml,
            Err(error) => {
                self.set_notice(error.to_string(), true);
                return;
            }
        };
        if !Path::new(&self.config.disk_path).exists() {
            if let Err(error) =
                libvirt::create_qcow2(Path::new(&self.config.disk_path), self.config.disk_size_gib)
            {
                self.set_notice(error.to_string(), true);
                return;
            }
        }
        if let Err(error) = libvirt::define_domain(&xml) {
            self.set_notice(error.to_string(), true);
            return;
        }
        if let Err(error) = save_config(&self.config) {
            self.set_notice(
                format!("VM was defined, but local settings could not be saved: {error}"),
                true,
            );
            return;
        }
        self.set_notice(
            if updating {
                "虚拟机配置已更新。"
            } else {
                "虚拟机已定义。现在可以启动并安装 Windows。"
            },
            false,
        );
        self.refresh();
    }

    fn vm_action(&mut self, action: &str) {
        if action == "start" {
            if let Err(error) = libvirt::validate_vm_start(&self.config) {
                self.set_notice(error.to_string(), true);
                return;
            }
        }
        match libvirt::run_control(action) {
            Ok(_) => {
                self.set_notice(
                    match action {
                        "start" => "虚拟机正在启动。",
                        "shutdown" => "已发送正常关机请求。",
                        "destroy" => "虚拟机已强制停止。",
                        _ => "操作已完成。",
                    },
                    false,
                );
                self.refresh();
            }
            Err(error) => self.set_notice(error.to_string(), true),
        }
    }

    fn toggle_pci(&mut self, device: &PciDevice, selected: bool) {
        if selected {
            if !self.config.pci_devices.contains(&device.address) {
                self.config.pci_devices.push(device.address.clone());
            }
        } else {
            self.config
                .pci_devices
                .retain(|address| address != &device.address);
        }
        self.config.pci_devices.sort();
    }

    fn toggle_usb(&mut self, device: &UsbDevice, selected: bool) {
        let selection = device.selection();
        if selected {
            if !self.config.usb_devices.contains(&selection) {
                self.config.usb_devices.push(selection);
            }
        } else {
            self.config.usb_devices.retain(|item| item != &selection);
        }
    }

    fn toggle_disk(&mut self, device: &BlockDevice, selected: bool) {
        if selected {
            if !self.config.physical_disks.contains(&device.path) {
                self.config.physical_disks.push(device.path.clone());
            }
        } else {
            self.config
                .physical_disks
                .retain(|path| path != &device.path);
        }
    }
}

impl eframe::App for WinshipApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_admin_task();
        if self.last_refresh.elapsed() >= Duration::from_secs(6) {
            self.refresh();
        }

        egui::SidePanel::left("navigation")
            .exact_width(226.0)
            .frame(
                egui::Frame::new()
                    .fill(NAV)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(context, |ui| self.navigation(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(context, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| match self.page {
                    Page::Overview => self.overview_page(ui),
                    Page::Host => self.host_page(ui),
                    Page::VirtualMachine => self.vm_page(ui),
                });
            });

        self.draw_force_stop_confirmation(context);
        self.draw_notice(context);
        context.request_repaint_after(Duration::from_millis(500));
    }
}

impl WinshipApp {
    fn navigation(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("W").size(24.0).strong().color(Color32::WHITE));
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("WINSHIP")
                        .size(16.0)
                        .strong()
                        .color(Color32::WHITE),
                );
                ui.label(
                    RichText::new("Windows VM")
                        .size(12.0)
                        .color(Color32::from_rgb(173, 187, 185)),
                );
            });
        });
        ui.add_space(26.0);
        ui.label(
            RichText::new("工作区")
                .size(11.0)
                .color(Color32::from_rgb(145, 161, 159)),
        );
        ui.add_space(8.0);
        self.nav_item(ui, Page::Overview, "◫", "概览");
        self.nav_item(ui, Page::VirtualMachine, "▣", "虚拟机");
        ui.add_space(16.0);
        ui.label(
            RichText::new("宿主机")
                .size(11.0)
                .color(Color32::from_rgb(145, 161, 159)),
        );
        ui.add_space(8.0);
        self.nav_item(ui, Page::Host, "⌘", "硬件与 IOMMU");

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(8.0);
            ui.label(
                RichText::new(&self.host.debian_release)
                    .size(11.0)
                    .color(Color32::from_rgb(145, 161, 159)),
            );
            ui.label(
                RichText::new("Debian · x86_64")
                    .size(11.0)
                    .color(Color32::from_rgb(145, 161, 159)),
            );
            ui.separator();
            ui.label(
                RichText::new("Winship 0.1.0")
                    .size(11.0)
                    .color(Color32::from_rgb(145, 161, 159)),
            );
        });
    }

    fn nav_item(&mut self, ui: &mut egui::Ui, page: Page, icon: &str, label: &str) {
        let selected = self.page == page;
        let fill = if selected {
            Color32::from_rgb(49, 71, 71)
        } else {
            NAV
        };
        let text = if selected {
            Color32::WHITE
        } else {
            Color32::from_rgb(196, 207, 204)
        };
        let button = egui::Button::new(
            RichText::new(format!("{icon}    {label}"))
                .size(14.0)
                .color(text),
        )
        .fill(fill)
        .stroke(Stroke::NONE)
        .min_size(egui::vec2(188.0, 42.0));
        if ui.add(button).clicked() {
            self.page = page;
        }
    }

    fn page_header(&mut self, ui: &mut egui::Ui, eyebrow: &str, title: &str, description: &str) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(eyebrow.to_uppercase())
                        .size(11.0)
                        .strong()
                        .color(GREEN),
                );
                ui.add_space(3.0);
                ui.heading(RichText::new(title).size(25.0).strong().color(TEXT));
                ui.label(RichText::new(description).size(13.0).color(MUTED));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let refresh = ui.button("↻  刷新");
                if refresh.clicked() {
                    self.refresh();
                }
            });
        });
        ui.add_space(22.0);
    }

    fn overview_page(&mut self, ui: &mut egui::Ui) {
        self.page_header(
            ui,
            "Winship / 单机管理",
            "Windows 虚拟机",
            "本地 QEMU / libvirt · Q35 · OVMF · VFIO",
        );

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(VM_NAME).size(16.0).strong().color(TEXT));
                    ui.add_space(4.0);
                    status_label(ui, &self.status.state);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let force_stop = ui
                        .add_enabled(self.status.state.is_active(), egui::Button::new("强制停止"));
                    if force_stop.clicked() {
                        self.force_stop_confirmation = true;
                    }
                    let shutdown = ui
                        .add_enabled(self.status.state.is_active(), egui::Button::new("正常关机"));
                    if shutdown.clicked() {
                        self.vm_action("shutdown");
                    }
                    let start = ui.add_enabled(
                        !self.status.state.is_active()
                            && !matches!(self.status.state, PowerState::NotDefined),
                        egui::Button::new(RichText::new("启动").strong().color(Color32::WHITE))
                            .fill(GREEN),
                    );
                    if start.clicked() {
                        self.vm_action("start");
                    }
                });
            });
        });
        ui.add_space(14.0);

        if let Some(error) = &self.status_error {
            alert(ui, "libvirt 尚未就绪", error, RED);
            ui.add_space(14.0);
        }
        if matches!(self.status.state, PowerState::NotDefined) {
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("还没有定义虚拟机").size(18.0).strong().color(TEXT));
                        ui.label(RichText::new("完成介质、内存和直通设备设置后，可以在这里定义 Windows VM。默认内存为 4 GiB，最大 12 GiB。")
                            .size(13.0).color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(RichText::new("配置虚拟机  →").color(Color32::WHITE).strong()).clicked() {
                            self.page = Page::VirtualMachine;
                        }
                    });
                });
            });
            ui.add_space(16.0);
        }

        ui.columns(3, |columns| {
            metric(
                &mut columns[0],
                "处理器",
                format!(
                    "{} vCPU",
                    self.status.cpu_count.unwrap_or(self.config.cpu_cores)
                ),
                &self
                    .cpu_usage_percent
                    .map(|percent| format!("{percent:.0}% 使用率"))
                    .unwrap_or_else(|| "host-passthrough".into()),
            );
            metric(
                &mut columns[1],
                "当前内存",
                self.status
                    .used_memory_kib
                    .map(format_kib)
                    .unwrap_or_else(|| format_mib(self.config.startup_memory_mib)),
                "VirtIO Balloon",
            );
            metric(
                &mut columns[2],
                "最大内存",
                self.status
                    .max_memory_kib
                    .map(format_kib)
                    .unwrap_or_else(|| format_mib(self.config.maximum_memory_mib)),
                "可由宿主机回收闲置内存",
            );
        });
        ui.add_space(18.0);

        ui.horizontal(|ui| {
            ui.heading(RichText::new("准备状态").size(17.0).strong().color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("查看硬件 →").clicked() {
                    self.page = Page::Host;
                }
            });
        });
        ui.add_space(8.0);
        let iommu_ok = self.host.iommu_enabled();
        let ovmf_ok = self.host.firmware.code.is_some() && self.host.firmware.vars.is_some();
        ui.columns(3, |columns| {
            readiness(
                &mut columns[0],
                "IOMMU 分组",
                iommu_ok,
                if iommu_ok {
                    format!("{} 个分组可用", self.host.iommu_groups.len())
                } else {
                    "未发现 IOMMU groups".into()
                },
            );
            readiness(
                &mut columns[1],
                "OVMF 固件",
                ovmf_ok,
                if ovmf_ok {
                    "UEFI 固件与 NVRAM 模板已就绪".into()
                } else {
                    "安装 Debian 软件包 ovmf".into()
                },
            );
            readiness(
                &mut columns[2],
                "PCI 直通设备",
                !self.config.pci_devices.is_empty(),
                format!("{} 个设备已选择", self.config.pci_devices.len()),
            );
        });

        ui.add_space(18.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("物理显示输出").strong().color(TEXT));
                ui.label(RichText::new("Windows 画面输出到直通 NVIDIA 显卡。请将显示器接到独显接口；Winship 不提供远程桌面回传。")
                    .size(13.0).color(MUTED));
            });
        });
    }

    fn host_page(&mut self, ui: &mut egui::Ui) {
        self.page_header(
            ui,
            "Host / PCI passthrough",
            "硬件与 IOMMU",
            "检查启动参数、分组边界、OVMF 和可直通设备",
        );

        let iommu_ok = self.host.iommu_enabled();
        let cmdline_ok = self.host.iommu_cmdline_enabled;
        if iommu_ok {
            alert(
                ui,
                "IOMMU groups 已发现",
                if cmdline_ok {
                    "内核启动参数包含 IOMMU 配置。"
                } else {
                    "内核已创建 IOMMU groups；启动参数中没有显式的 intel_iommu=on / amd_iommu=on。"
                },
                GREEN,
            );
        } else {
            alert(
                ui,
                "IOMMU 尚未启用",
                "检查 BIOS/UEFI 中的 VT-d 或 AMD-Vi，并将下方参数加入 GRUB_CMDLINE_LINUX_DEFAULT。保存后运行 update-grub 并重启。",
                RED,
            );
        }
        match self.host.secure_boot_enabled {
            Some(true) => {
                alert(
                    ui,
                    "Secure Boot 已启用",
                    "Winship 的 VFIO 配置要求关闭 Secure Boot。请在固件设置中关闭后重新检查。",
                    RED,
                );
                ui.add_space(8.0);
            }
            Some(false) => {
                ui.label(RichText::new("Secure Boot 已关闭").size(12.0).color(GREEN));
            }
            None => {
                ui.label(
                    RichText::new("无法从 EFI 变量确认 Secure Boot 状态")
                        .size(12.0)
                        .color(AMBER),
                );
            }
        }
        ui.add_space(8.0);
        let mut iommu_hint = self.host.iommu_hint().to_string();
        ui.horizontal(|ui| {
            ui.label(RichText::new("GRUB 参数").strong().color(TEXT));
            ui.add(
                egui::TextEdit::singleline(&mut iommu_hint)
                    .desired_width(260.0)
                    .interactive(false),
            );
            if ui.button("复制参数").clicked() {
                ui.ctx().copy_text(iommu_hint.clone());
            }
            if self.host.iommu_cmdline_enabled {
                ui.label(RichText::new("当前命令行：").color(MUTED));
            }
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!("/proc/cmdline  {}", self.host.kernel_cmdline))
                .size(11.0)
                .color(MUTED),
        );
        ui.add_space(16.0);

        ui.horizontal(|ui| {
            ui.heading(RichText::new("OVMF 固件").size(16.0).strong().color(TEXT));
            ui.label(
                if self.host.firmware.code.is_some() && self.host.firmware.vars.is_some() {
                    RichText::new("已就绪").color(GREEN)
                } else {
                    RichText::new("缺少文件").color(RED)
                },
            );
        });
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(format!("CODE: {}", display_path(&self.host.firmware.code)));
            ui.label(format!("VARS: {}", display_path(&self.host.firmware.vars)));
            if self.host.firmware.code.is_none() || self.host.firmware.vars.is_none() {
                ui.label(RichText::new("安装 ovmf：sudo apt install ovmf").color(AMBER));
            }
        });
        ui.add_space(16.0);

        ui.horizontal(|ui| {
            ui.heading(RichText::new(format!("PCI 设备  {}", self.host.pci_devices.len())).size(16.0).strong().color(TEXT));
            ui.label(RichText::new("NVIDIA GPU/音频与 USB 控制器会显示完整 IOMMU group。选择动作在虚拟机页面完成。")
                .size(12.0).color(MUTED));
        });
        ui.add_space(6.0);
        let pci_devices = self.host.pci_devices.clone();
        for device in pci_devices {
            let is_target = device.is_nvidia() || device.is_usb_controller() || device.is_audio();
            if !is_target {
                continue;
            }
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&device.description).strong().color(TEXT));
                        ui.label(
                            RichText::new(format!(
                                "{}  ·  {}:{}  ·  {}",
                                device.address,
                                device.vendor_id,
                                device.device_id,
                                device.driver.as_deref().unwrap_or("no driver")
                            ))
                            .monospace()
                            .size(11.0)
                            .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "IOMMU group {}",
                                device.iommu_group.map_or("—".into(), |id| id.to_string())
                            ))
                            .size(11.0)
                            .color(BLUE),
                        );
                        let selected = self.config.pci_devices.contains(&device.address);
                        if selected {
                            ui.label(RichText::new("已选择").color(GREEN));
                        }
                    });
                });
                if let Some(group) = device.iommu_group {
                    let members = self.host.group_members(Some(group));
                    ui.label(
                        RichText::new(format!(
                            "同组设备：{}",
                            if members.is_empty() {
                                "未能读取".into()
                            } else {
                                members.join("  ·  ")
                            }
                        ))
                        .size(11.0)
                        .color(MUTED),
                    );
                }
            });
        }
        if self
            .host
            .pci_devices
            .iter()
            .all(|device| !device.is_nvidia() && !device.is_usb_controller())
        {
            ui.label(RichText::new("未发现 NVIDIA GPU 或 PCIe USB 控制器。可安装 pciutils 获取设备名称；PCI 地址仍由 sysfs 枚举。")
                .color(AMBER));
        }
        ui.add_space(14.0);

        ui.heading(
            RichText::new(format!("USB 设备  {}", self.host.usb_devices.len()))
                .size(16.0)
                .strong()
                .color(TEXT),
        );
        ui.add_space(6.0);
        if self.host.usb_devices.is_empty() {
            ui.label(
                RichText::new("当前没有发现可选的 USB 设备。可在虚拟机页面添加已连接的设备。")
                    .size(12.0)
                    .color(MUTED),
            );
        } else {
            for device in &self.host.usb_devices {
                ui.label(format!(
                    "{}  ·  Bus {:03} Device {:03}",
                    device.display_name(),
                    device.bus,
                    device.address
                ));
            }
        }
        ui.add_space(12.0);

        ui.heading(
            RichText::new(format!("可移动磁盘  {}", self.host.block_devices.len()))
                .size(16.0)
                .strong()
                .color(TEXT),
        );
        ui.add_space(6.0);
        if self.host.block_devices.is_empty() {
            ui.label(
                RichText::new("未发现 USB 或 removable 块设备。lsblk 不可用时请安装 util-linux。")
                    .size(12.0)
                    .color(MUTED),
            );
        } else {
            for device in &self.host.block_devices {
                ui.label(format!(
                    "{}  ·  {}  ·  {}",
                    device.path, device.model, device.size
                ));
            }
        }
        ui.add_space(16.0);

        ui.heading(
            RichText::new("应用 VFIO 配置")
                .size(16.0)
                .strong()
                .color(TEXT),
        );
        ui.label(RichText::new("只会写入已选择设备的 vendor:device ID，并更新 Debian initramfs。操作不会修改 GRUB 或自动重启。")
            .size(12.0).color(MUTED));
        let selected_ids = self
            .host
            .selected_vfio_ids(&self.config)
            .unwrap_or_default();
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            if selected_ids.is_empty() {
                ui.label(RichText::new("尚未选择 PCI 设备。请先到“虚拟机”页面选择 GPU、音频功能和 USB 控制器。")
                    .color(AMBER));
            } else {
                ui.label(RichText::new(format!("vfio-pci IDs: {}", selected_ids.join(","))).monospace().color(TEXT));
                if let Ok(conflicts) = self.host.selected_vfio_conflicts(&self.config) {
                    if !conflicts.is_empty() {
                        ui.label(RichText::new(format!("这些 ID 还匹配未选择的设备：{}。vfio-pci 会按 ID 绑定所有匹配项。", conflicts.join(", ")))
                            .color(RED));
                    }
                }
                let incomplete = self.config.pci_devices.iter().filter_map(|address| {
                    let device = self.host.pci_devices.iter().find(|device| device.address == *address)?;
                    (!self.host.selected_group_is_complete(&self.config, device.iommu_group)).then_some(device.iommu_group)
                }).collect::<Vec<_>>();
                if !incomplete.is_empty() {
                    ui.label(RichText::new("至少一个 IOMMU group 未完整选择。请在虚拟机页面选择该组所有设备。")
                        .color(RED));
                }
            }
            let can_apply = !selected_ids.is_empty() && !self.admin_busy;
            let button = ui.add_enabled(can_apply, egui::Button::new(if self.admin_busy { "正在等待系统授权…" } else { "写入 /etc/modprobe.d/vfio.conf 并更新 initramfs" }));
            if button.clicked() {
                self.start_apply_vfio();
            }
            ui.label(RichText::new("特权操作通过 PolicyKit 执行；成功后请重启主机。当前主机的 GPU 不会在本次运行中被解绑。")
                .size(11.0).color(MUTED));
        });
    }

    fn vm_page(&mut self, ui: &mut egui::Ui) {
        let existing = !matches!(self.status.state, PowerState::NotDefined);
        self.page_header(
            ui,
            "Virtual machine / libvirt",
            "虚拟机配置",
            "新建或编辑单台 Windows VM 的安装介质、资源与直通设备",
        );

        if self.status.state.is_active() {
            alert(
                ui,
                "虚拟机正在运行",
                "保存硬件配置前请先在 Windows 中正常关机。强制停止可能造成文件系统损坏。",
                AMBER,
            );
            ui.add_space(12.0);
        }

        ui.horizontal(|ui| {
            step_label(ui, "01", "安装介质", true);
            step_label(ui, "02", "CPU 与内存", true);
            step_label(ui, "03", "直通设备", true);
        });
        ui.add_space(12.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("安装介质与虚拟磁盘").strong().color(TEXT));
            ui.add_space(8.0);
            path_picker(ui, "Windows 安装 ISO", &mut self.config.windows_iso, PathKind::Iso);
            path_picker(ui, "VirtIO 驱动 ISO", &mut self.config.virtio_iso, PathKind::Iso);
            path_picker(ui, "新建 qcow2 磁盘", &mut self.config.disk_path, PathKind::Disk);
            ui.horizontal(|ui| {
                ui.label(RichText::new("容量").color(MUTED));
                ui.add(egui::DragValue::new(&mut self.config.disk_size_gib).range(16..=4096).suffix(" GiB"));
                ui.label(RichText::new("磁盘仅在首次定义 VM 时创建。安装 Windows 时从 VirtIO ISO 加载存储和网络驱动。")
                    .size(11.0).color(MUTED));
            });
        });
        ui.add_space(12.0);

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("计算资源").strong().color(TEXT));
            ui.add_space(8.0);
            ui.columns(3, |columns| {
                columns[0].label("vCPU 核心");
                columns[0].add(egui::DragValue::new(&mut self.config.cpu_cores).range(1..=256));
                columns[0].label(
                    RichText::new("QEMU host-passthrough")
                        .size(11.0)
                        .color(MUTED),
                );

                columns[1].label("启动内存");
                columns[1].add(
                    egui::DragValue::new(&mut self.config.startup_memory_mib)
                        .range(1024..=1_048_576)
                        .speed(256)
                        .suffix(" MiB"),
                );
                columns[1].label(
                    RichText::new("当前分配，可被气球回收")
                        .size(11.0)
                        .color(MUTED),
                );

                columns[2].label("最大内存");
                columns[2].add(
                    egui::DragValue::new(&mut self.config.maximum_memory_mib)
                        .range(1024..=1_048_576)
                        .speed(256)
                        .suffix(" MiB"),
                );
                columns[2].label(
                    RichText::new("VM 上限 · VirtIO Balloon")
                        .size(11.0)
                        .color(MUTED),
                );
            });
            ui.label(
                RichText::new("Windows 中需要安装 VirtIO Balloon 驱动，宿主机才能回收未使用内存。")
                    .size(11.0)
                    .color(AMBER),
            );
        });
        ui.add_space(12.0);

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("PCIe 设备直通").strong().color(TEXT));
                ui.label(
                    RichText::new("GPU 及其音频功能、整卡 USB 控制器")
                        .size(11.0)
                        .color(MUTED),
                );
            });
            ui.label(
                RichText::new(
                    "同一 IOMMU group 的所有设备需要一并选择。PCIe 设备会挂到 Q35 PCIe root port。",
                )
                .size(11.0)
                .color(MUTED),
            );
            ui.add_space(6.0);
            let pci_devices = self.host.pci_devices.clone();
            let targets = pci_devices
                .iter()
                .filter(|device| device.is_nvidia() || device.is_usb_controller())
                .collect::<Vec<_>>();
            if targets.is_empty() {
                ui.label(
                    RichText::new(
                        "未发现 NVIDIA GPU 或 PCIe USB 控制器。设备列表仍可在硬件页面查看。",
                    )
                    .color(AMBER),
                );
            }
            for device in targets {
                let mut selected = self.config.pci_devices.contains(&device.address);
                let label = format!(
                    "{}  ·  {}  ·  {}",
                    device.address,
                    device.description,
                    device
                        .iommu_group
                        .map_or_else(|| "no IOMMU group".into(), |group| format!("group {group}"))
                );
                if ui.checkbox(&mut selected, label).changed() {
                    self.toggle_pci(device, selected);
                }
                if let Some(group) = device.iommu_group {
                    let members = self.host.group_members(Some(group)).to_vec();
                    let complete = self
                        .host
                        .selected_group_is_complete(&self.config, Some(group));
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        ui.label(
                            RichText::new(format!(
                                "Group {group}: {}",
                                if members.is_empty() {
                                    "unknown members".into()
                                } else {
                                    members.join(", ")
                                }
                            ))
                            .size(10.0)
                            .color(if complete {
                                GREEN
                            } else {
                                MUTED
                            }),
                        );
                        if !complete
                            && !members.is_empty()
                            && ui.small_button("选择整个组").clicked()
                        {
                            let currently_selected = self.config.pci_devices.clone();
                            self.config.pci_devices.extend(
                                members
                                    .into_iter()
                                    .filter(|member| !currently_selected.contains(member)),
                            );
                            self.config.pci_devices.sort();
                        }
                    });
                }
            }
        });
        ui.add_space(12.0);

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("单个 USB 设备").strong().color(TEXT));
            ui.label(RichText::new("按当前 bus/device 地址和 vendor/product 匹配。设备拔插后地址可能变化，请刷新设备列表并重新选择。")
                .size(11.0).color(MUTED));
            ui.add_space(6.0);
            let usb_devices = self.host.usb_devices.clone();
            for device in &usb_devices {
                let mut selected = self.config.usb_devices.contains(&device.selection());
                if ui.checkbox(&mut selected, format!("{}  ·  bus {} device {}", device.display_name(), device.bus, device.address)).changed() {
                    self.toggle_usb(device, selected);
                }
            }
            if usb_devices.is_empty() {
                ui.label(RichText::new("连接 USB 设备后刷新宿主机扫描。")
                    .size(11.0).color(MUTED));
            }
        });
        ui.add_space(12.0);

        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("物理移動磁盘 / 整盘直通").strong().color(TEXT));
            ui.label(RichText::new("直通后该磁盘在 Debian 宿主机侧不可见，不能同时挂载。请先卸载磁盘上的所有分区。")
                .color(AMBER).size(12.0));
            ui.add_space(6.0);
            let block_devices = self.host.block_devices.clone();
            for device in &block_devices {
                let mut selected = self.config.physical_disks.contains(&device.path);
                let label = format!("{}  ·  {}  ·  {}  ·  {}", device.path, device.model, device.size, device.transport);
                if ui.checkbox(&mut selected, label).changed() {
                    self.toggle_disk(device, selected);
                }
            }
            if block_devices.is_empty() {
                ui.label(RichText::new("未发现可移动磁盘。插入目标磁盘后刷新硬件页面。")
                    .size(11.0).color(MUTED));
            }
        });
        ui.add_space(16.0);

        if existing && self.status.state.is_active() {
            ui.label(
                RichText::new("虚拟机运行时不能保存硬件配置。请先正常关机。")
                    .size(12.0)
                    .color(AMBER),
            );
        }
        let enabled = !self.status.state.is_active();
        let label = if existing {
            "保存虚拟机配置"
        } else {
            "定义 Windows 虚拟机"
        };
        if ui
            .add_enabled(
                enabled,
                egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                    .fill(GREEN)
                    .min_size(egui::vec2(220.0, 42.0)),
            )
            .clicked()
        {
            self.define_vm();
        }
        if existing {
            ui.label(RichText::new("配置固定为 Q35 + OVMF，VM 名称为 winship-windows。编辑 PCI 直通后需要关机并重新启动。")
                .size(11.0).color(MUTED));
        }
    }

    fn draw_force_stop_confirmation(&mut self, context: &egui::Context) {
        if !self.force_stop_confirmation {
            return;
        }
        egui::Window::new("强制停止虚拟机")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(context, |ui| {
                ui.label("强制停止会立即切断虚拟机电源，可能损坏 Windows 文件系统或未保存的数据。继续吗？");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() {
                        self.force_stop_confirmation = false;
                    }
                    if ui.add(egui::Button::new(RichText::new("强制停止").color(Color32::WHITE)).fill(RED)).clicked() {
                        self.force_stop_confirmation = false;
                        self.vm_action("destroy");
                    }
                });
            });
    }

    fn draw_notice(&mut self, context: &egui::Context) {
        let Some(notice) = &self.notice else {
            return;
        };
        if !self.admin_busy && notice.created.elapsed() > Duration::from_secs(8) {
            self.notice = None;
            return;
        }
        let color = if notice.is_error { RED } else { GREEN };
        egui::Area::new(egui::Id::new("winship-notice"))
            .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-24.0, -20.0))
            .show(context, |ui| {
                egui::Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0_f32, color))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_max_width(460.0);
                        ui.label(RichText::new(&notice.message).color(TEXT));
                    });
            });
    }
}

#[derive(Clone, Copy)]
enum PathKind {
    Iso,
    Disk,
}

fn path_picker(ui: &mut egui::Ui, label: &str, value: &mut String, kind: PathKind) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(MUTED));
        let input_width = (ui.available_width() - 112.0).max(100.0);
        ui.add(egui::TextEdit::singleline(value).desired_width(input_width));
        let button = match kind {
            PathKind::Iso => "选择 ISO…",
            PathKind::Disk => "选择路径…",
        };
        if ui.button(button).clicked() {
            let selected = match kind {
                PathKind::Iso => rfd::FileDialog::new()
                    .add_filter("ISO image", &["iso"])
                    .pick_file(),
                PathKind::Disk => rfd::FileDialog::new()
                    .set_directory("/var/lib/libvirt/images")
                    .set_file_name("winship-windows.qcow2")
                    .save_file(),
            };
            if let Some(path) = selected {
                *value = path.to_string_lossy().into_owned();
            }
        }
    });
}

fn status_label(ui: &mut egui::Ui, state: &PowerState) {
    let (color, label) = match state {
        PowerState::Running => (GREEN, "运行中"),
        PowerState::Paused => (AMBER, "已暂停"),
        PowerState::ShutOff => (MUTED, "已关闭"),
        PowerState::NotDefined => (AMBER, "未定义"),
        PowerState::Other(_) => (BLUE, state.label()),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("●").color(color));
        ui.label(RichText::new(label).strong().color(color));
    });
}

fn metric(ui: &mut egui::Ui, title: &str, value: String, note: &str) {
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new(title).size(12.0).color(MUTED));
        ui.add_space(4.0);
        ui.label(RichText::new(value).size(21.0).strong().color(TEXT));
        ui.label(RichText::new(note).size(11.0).color(MUTED));
    });
}

fn readiness(ui: &mut egui::Ui, title: &str, okay: bool, detail: String) {
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("●").color(if okay { GREEN } else { AMBER }));
            ui.label(RichText::new(title).strong().color(TEXT));
        });
        ui.label(RichText::new(detail).size(11.0).color(MUTED));
    });
}

fn alert(ui: &mut egui::Ui, title: &str, detail: &str, color: Color32) {
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new("●").color(color));
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong().color(color));
                ui.label(RichText::new(detail).size(12.0).color(TEXT));
            });
        });
    });
}

fn step_label(ui: &mut egui::Ui, number: &str, label: &str, enabled: bool) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(number)
                .monospace()
                .strong()
                .color(if enabled { GREEN } else { MUTED }),
        );
        ui.label(RichText::new(label).color(if enabled { TEXT } else { MUTED }));
    });
    ui.add_space(18.0);
}

fn format_mib(mib: u32) -> String {
    if mib % 1024 == 0 {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{mib} MiB")
    }
}

fn format_kib(kib: u64) -> String {
    format_mib((kib / 1024) as u32)
}

fn display_path(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "未找到".into())
}

fn config_file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("winship").join("vm.json")
}

fn load_config() -> VmConfig {
    std::fs::read(config_file())
        .ok()
        .and_then(|contents| serde_json::from_slice(&contents).ok())
        .unwrap_or_default()
}

fn save_config(config: &VmConfig) -> std::io::Result<()> {
    let path = config_file();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(config).expect("serializing VmConfig cannot fail"),
    )?;
    std::fs::rename(temporary, path)
}

fn configure_theme(context: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = Color32::from_rgb(248, 250, 249);
    visuals.selection.bg_fill = Color32::from_rgb(202, 231, 217);
    visuals.selection.stroke = Stroke::new(1.0_f32, GREEN);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(246, 248, 247);
    visuals.widgets.inactive.fg_stroke.color = TEXT;
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(231, 239, 235);
    visuals.widgets.active.bg_fill = GREEN;
    visuals.widgets.active.fg_stroke.color = Color32::WHITE;
    context.set_visuals(visuals);

    for font_path in [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(font_path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "winship-cjk".into(),
                egui::FontData::from_owned(bytes).into(),
            );
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "winship-cjk".into());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .insert(0, "winship-cjk".into());
            context.set_fonts(fonts);
            break;
        }
    }
    let mut style = (*context.style()).clone();
    style.text_styles.insert(
        egui::TextStyle::Heading,
        FontId::new(24.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(13.0, FontFamily::Proportional),
    );
    context.set_style(style);
}
