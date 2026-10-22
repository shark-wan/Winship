use cxx_qt_lib::QString;
use std::pin::Pin;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    #[auto_cxx_name]
    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qml_singleton]
        #[qproperty(QString, state_json)]
        type Winship = super::WinshipRust;

        #[qinvokable]
        fn refresh(self: Pin<&mut Self>);

        #[qinvokable]
        fn version(&self) -> QString;
    }
}

pub struct WinshipRust {
    state_json: QString,
}

impl Default for WinshipRust {
    fn default() -> Self {
        Self {
            state_json: QString::from("{\"status\":\"initializing\"}"),
        }
    }
}

impl qobject::Winship {
    pub fn refresh(self: Pin<&mut Self>) {
        let host = winship::host::inspect_host();
        let state = serde_json::json!({
            "status": "ok",
            "release": host.debian_release,
            "kernelCmdline": host.kernel_cmdline,
            "iommuEnabled": host.iommu_enabled(),
            "iommuCmdline": host.iommu_cmdline_enabled,
            "secureBoot": host.secure_boot_enabled,
            "pciCount": host.pci_devices.len(),
            "usbCount": host.usb_devices.len(),
            "diskCount": host.block_devices.len(),
            "networkCount": host.network_interfaces.len(),
            "ovmfReady": host.firmware.code.is_some() && host.firmware.vars.is_some(),
        });
        self.set_state_json(QString::from(state.to_string()));
    }

    pub fn version(&self) -> QString {
        QString::from(env!("CARGO_PKG_VERSION"))
    }
}
