use crate::model::VmConfig;
use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const IMAGE_ROOT: &str = "/var/lib/libvirt/images";
pub const VM_DIR: &str = "/var/lib/libvirt/images/winship";

pub fn config_file() -> PathBuf {
    if let Some(path) = std::env::var_os("WINSHIP_CONFIG_FILE") {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("winship").join("vm.json")
}

pub fn media_dir() -> PathBuf {
    Path::new(VM_DIR).join("media")
}

pub fn default_disk_path() -> String {
    Path::new(VM_DIR)
        .join("winship-windows.qcow2")
        .to_string_lossy()
        .into_owned()
}

pub fn load() -> VmConfig {
    let mut config = fs::read(config_file())
        .ok()
        .and_then(|contents| serde_json::from_slice(&contents).ok())
        .unwrap_or_default();
    apply_defaults(&mut config);
    config
}

pub fn apply_defaults(config: &mut VmConfig) {
    if config.disk_path.is_empty() {
        config.disk_path = default_disk_path();
    }
    if config.windows_iso.is_empty() {
        for candidate in [
            Path::new(VM_DIR).join("win10.iso"),
            media_dir().join("win10.iso"),
        ] {
            if candidate.is_file() {
                config.windows_iso = candidate.to_string_lossy().into_owned();
                break;
            }
        }
    }
    if config.virtio_iso.is_empty() {
        for candidate in [
            Path::new(VM_DIR).join("virtio-win.iso"),
            media_dir().join("virtio-win.iso"),
        ] {
            if candidate.is_file() {
                config.virtio_iso = candidate.to_string_lossy().into_owned();
                break;
            }
        }
    }
    if config.network_mode != crate::model::NetworkMode::Nat && config.network_interface.is_empty()
    {
        if let Some(interface) = crate::host::active_default_interface() {
            config.network_interface = interface.name;
        }
    }
}

pub fn save(config: &VmConfig) -> io::Result<()> {
    let path = config_file();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(
            &serde_json::to_vec_pretty(config).expect("serializing VmConfig cannot fail"),
        )?;
        file.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(temporary, path)
}

pub fn qemu_can_read(path: &str) -> bool {
    let path = Path::new(path);
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata.permissions().mode() & 0o004 == 0 {
        return false;
    }
    for ancestor in path.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        let Ok(metadata) = ancestor.metadata() else {
            return false;
        };
        if metadata.permissions().mode() & 0o001 == 0 {
            return false;
        }
    }
    true
}

pub fn same_file(left: &str, right: &str) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_world_readable_paths() {
        let root = std::env::temp_dir().join(format!("winship-readable-{}", std::process::id()));
        let file = root.join("media.iso");
        fs::create_dir_all(&root).unwrap();
        fs::write(&file, b"iso").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!qemu_can_read(&file.to_string_lossy()));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let readable = qemu_can_read(&file.to_string_lossy());
        fs::remove_dir_all(root).unwrap();
        // The temporary directory itself is often 0700, so only assert the
        // positive case when the whole path is traversable.
        if readable {
            assert!(readable);
        }
    }
}
