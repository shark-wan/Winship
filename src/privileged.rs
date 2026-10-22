use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

const MANAGED_BEGIN: &str = "# BEGIN WINSHIP MANAGED VFIO";
const MANAGED_END: &str = "# END WINSHIP MANAGED VFIO";
const VFIO_MODULES: [&str; 3] = ["vfio_pci", "vfio", "vfio_iommu_type1"];

#[derive(Debug, Error)]
pub enum PrivilegedError {
    #[error("At least one valid vendor:device ID is required")]
    InvalidIds,
    #[error(
        "An existing modprobe configuration contains an unmanaged vfio-pci ids= rule; review it manually before using Winship VFIO setup"
    )]
    ExistingVfioIds,
    #[error("Could not update {path}: {source}")]
    Write { path: String, source: io::Error },
    #[error("update-initramfs -u -k all failed: {0}")]
    Initramfs(String),
    #[error("Privileged command failed: {0}")]
    Command(String),
    #[error("Could not start PolicyKit: {0}")]
    PolicyKit(io::Error),
}

#[derive(Clone)]
struct FileSnapshot {
    content: Option<Vec<u8>>,
    mode: Option<u32>,
}

pub fn validate_vfio_ids(ids: &[String]) -> bool {
    !ids.is_empty()
        && ids.iter().all(|id| {
            let Some((vendor, device)) = id.split_once(':') else {
                return false;
            };
            vendor.len() == 4
                && device.len() == 4
                && vendor.bytes().all(|byte| byte.is_ascii_hexdigit())
                && device.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

pub fn vfio_conf_with_ids(existing: &str, ids: &[String]) -> String {
    let id_list = ids
        .iter()
        .map(|id| id.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(",");
    let mut block = format!(
        "{MANAGED_BEGIN}\noptions vfio-pci ids={id_list}\nsoftdep nouveau pre: vfio-pci\nsoftdep xhci_pci pre: vfio-pci"
    );
    if ids.iter().any(|id| id.starts_with("10de:")) {
        block.push_str("\nblacklist nouveau");
    }
    block.push_str(&format!("\n{MANAGED_END}"));
    replace_managed_block(existing, &block)
}

pub fn initramfs_modules_with_vfio(existing: &str) -> String {
    let mut result = existing.trim_end_matches('\n').to_string();
    for module in VFIO_MODULES {
        if !existing.lines().any(|line| line.trim() == module) {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(module);
        }
    }
    if !result.is_empty() {
        result.push('\n');
    }
    result
}

fn replace_managed_block(existing: &str, replacement: &str) -> String {
    let lines = existing.lines().collect::<Vec<_>>();
    let begin = lines.iter().position(|line| *line == MANAGED_BEGIN);
    let end = lines.iter().position(|line| *line == MANAGED_END);
    if let Some((begin, end)) = match (begin, end) {
        (Some(begin), Some(end)) if begin <= end => Some((begin, end)),
        _ => None,
    } {
        let mut output = lines[..begin].join("\n");
        if begin > 0 {
            output.push('\n');
        }
        output.push_str(replacement);
        if end + 1 < lines.len() {
            output.push('\n');
            output.push_str(&lines[end + 1..].join("\n"));
        }
        if existing.ends_with('\n') && !output.ends_with('\n') {
            output.push('\n');
        }
        return output;
    }
    let mut output = existing.trim_end_matches('\n').to_string();
    if !output.is_empty() {
        output.push_str("\n\n");
    }
    output.push_str(replacement);
    output.push('\n');
    output
}

pub fn apply_vfio(ids: &[String]) -> Result<(), PrivilegedError> {
    if !validate_vfio_ids(ids) {
        return Err(PrivilegedError::InvalidIds);
    }
    apply_vfio_to_paths(
        ids,
        Path::new("/etc/modprobe.d/vfio.conf"),
        Path::new("/etc/initramfs-tools/modules"),
    )
}

pub fn create_disk(path: &Path, size_gib: u16) -> Result<(), PrivilegedError> {
    if !(16..=4096).contains(&size_gib) || !path.is_absolute() || path.exists() {
        return Err(PrivilegedError::Write {
            path: path.display().to_string(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "Disk size, path, or target is invalid",
            ),
        });
    }
    let allowed_parent = Path::new("/var/lib/libvirt/images");
    let parent = path.parent().ok_or_else(|| PrivilegedError::Write {
        path: path.display().to_string(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "Disk path has no parent"),
    })?;
    let canonical_parent = fs::canonicalize(parent).map_err(|source| PrivilegedError::Write {
        path: parent.display().to_string(),
        source,
    })?;
    if canonical_parent != allowed_parent || path.file_name().is_none() {
        return Err(PrivilegedError::Write {
            path: path.display().to_string(),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Winship can create virtual disks only in /var/lib/libvirt/images",
            ),
        });
    }
    let output = Command::new("qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(path)
        .arg(format!("{size_gib}G"))
        .output()
        .map_err(|source| PrivilegedError::Write {
            path: path.display().to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(PrivilegedError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    if let Err(source) = fs::set_permissions(path, Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(path);
        return Err(PrivilegedError::Write {
            path: path.display().to_string(),
            source,
        });
    }
    Ok(())
}

fn apply_vfio_to_paths(
    ids: &[String],
    vfio_path: &Path,
    modules_path: &Path,
) -> Result<(), PrivilegedError> {
    let vfio_snapshot = snapshot(vfio_path).map_err(|source| PrivilegedError::Write {
        path: vfio_path.display().to_string(),
        source,
    })?;
    let modules_snapshot = snapshot(modules_path).map_err(|source| PrivilegedError::Write {
        path: modules_path.display().to_string(),
        source,
    })?;

    let existing_vfio = vfio_snapshot
        .content
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    let existing_modules = modules_snapshot
        .content
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    if has_unmanaged_vfio_ids(&existing_vfio)
        || has_external_vfio_ids(
            vfio_path
                .parent()
                .unwrap_or_else(|| Path::new("/etc/modprobe.d")),
            vfio_path,
        )
    {
        return Err(PrivilegedError::ExistingVfioIds);
    }
    write_atomic(
        vfio_path,
        vfio_conf_with_ids(&existing_vfio, ids).as_bytes(),
        vfio_snapshot.mode.unwrap_or(0o644),
    )
    .map_err(|source| PrivilegedError::Write {
        path: vfio_path.display().to_string(),
        source,
    })?;
    if let Err(source) = write_atomic(
        modules_path,
        initramfs_modules_with_vfio(&existing_modules).as_bytes(),
        modules_snapshot.mode.unwrap_or(0o644),
    ) {
        restore(vfio_path, &vfio_snapshot);
        return Err(PrivilegedError::Write {
            path: modules_path.display().to_string(),
            source,
        });
    }

    let output = Command::new("update-initramfs")
        .args(["-u", "-k", "all"])
        .output();
    let result = match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(PrivilegedError::Initramfs(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )),
        Err(error) => Err(PrivilegedError::Initramfs(error.to_string())),
    };
    if let Err(error) = result {
        restore(vfio_path, &vfio_snapshot);
        restore(modules_path, &modules_snapshot);
        let _ = Command::new("update-initramfs")
            .args(["-u", "-k", "all"])
            .output();
        return Err(error);
    }
    Ok(())
}

fn has_unmanaged_vfio_ids(existing: &str) -> bool {
    let mut managed = false;
    for line in existing.lines() {
        if line.trim() == MANAGED_BEGIN {
            managed = true;
            continue;
        }
        if line.trim() == MANAGED_END {
            managed = false;
            continue;
        }
        if !managed {
            let mut tokens = line.split_whitespace();
            if tokens.next() == Some("options")
                && tokens.next() == Some("vfio-pci")
                && tokens.any(|option| option.starts_with("ids="))
            {
                return true;
            }
        }
    }
    false
}

fn has_external_vfio_ids(directory: &Path, managed_path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path != managed_path
            && path
                .extension()
                .is_some_and(|extension| extension == "conf")
            && fs::read_to_string(path)
                .ok()
                .is_some_and(|content| has_unmanaged_vfio_ids(&content))
    })
}

fn snapshot(path: &Path) -> io::Result<FileSnapshot> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(FileSnapshot {
            content: Some(fs::read(path)?),
            mode: Some(metadata.permissions().mode() & 0o7777),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(FileSnapshot {
            content: None,
            mode: None,
        }),
        Err(error) => Err(error),
    }
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("/"));
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    let temp_path = parent.join(format!(".{filename}.winship-{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp_path)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        file.set_permissions(Permissions::from_mode(mode))?;
        fs::rename(&temp_path, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn restore(path: &Path, snapshot: &FileSnapshot) {
    match &snapshot.content {
        Some(content) => {
            let _ = write_atomic(path, content, snapshot.mode.unwrap_or(0o644));
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn request_vfio_apply(ids: &[String]) -> Result<String, PrivilegedError> {
    if !validate_vfio_ids(ids) {
        return Err(PrivilegedError::InvalidIds);
    }
    let executable = std::env::current_exe().map_err(PrivilegedError::PolicyKit)?;
    let id_list = ids.join(",");
    let output = Command::new("pkexec")
        .arg(executable)
        .args(["privileged", "apply-vfio", &id_list])
        .output()
        .map_err(PrivilegedError::PolicyKit)?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(PrivilegedError::Command(if error.is_empty() {
            "The privileged operation was cancelled or failed.".into()
        } else {
            error
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn request_create_disk(path: &Path, size_gib: u16) -> Result<(), PrivilegedError> {
    if !(16..=4096).contains(&size_gib)
        || !path.is_absolute()
        || path.parent() != Some(Path::new("/var/lib/libvirt/images"))
        || path.exists()
    {
        return Err(PrivilegedError::Write {
            path: path.display().to_string(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "Choose a new disk path in /var/lib/libvirt/images and a size from 16 to 4096 GiB",
            ),
        });
    }
    let executable = std::env::current_exe().map_err(PrivilegedError::PolicyKit)?;
    let output = Command::new("pkexec")
        .arg(executable)
        .args([
            "privileged",
            "create-disk",
            &size_gib.to_string(),
            &path.to_string_lossy(),
        ])
        .output()
        .map_err(PrivilegedError::PolicyKit)?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(PrivilegedError::Command(if error.is_empty() {
            "The privileged disk creation was cancelled or failed.".into()
        } else {
            error
        }));
    }
    Ok(())
}

pub fn parse_ids_argument(argument: &str) -> Option<Vec<String>> {
    let ids = argument
        .split(',')
        .map(|id| id.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    validate_vfio_ids(&ids).then_some(ids)
}

pub fn config_paths() -> (PathBuf, PathBuf) {
    (
        PathBuf::from("/etc/modprobe.d/vfio.conf"),
        PathBuf::from("/etc/initramfs-tools/modules"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_normalizes_vfio_ids() {
        assert!(validate_vfio_ids(&["10DE:2684".into(), "10de:22ba".into()]));
        assert!(!validate_vfio_ids(&[]));
        assert!(!validate_vfio_ids(&["10de:xyz".into()]));
        assert_eq!(
            parse_ids_argument("10DE:2684,10de:22ba"),
            Some(vec!["10de:2684".into(), "10de:22ba".into()])
        );
        assert_eq!(parse_ids_argument("10de:2684,;touch /tmp/x"), None);
    }

    #[test]
    fn managed_vfio_section_preserves_other_configuration_and_is_repeatable() {
        let existing = "options vfio-pci disable_vga=1\n\n# local comment\n";
        let ids = vec!["10de:2684".into(), "10de:22ba".into()];
        let first = vfio_conf_with_ids(existing, &ids);
        let second = vfio_conf_with_ids(&first, &["10de:2684".into()]);
        assert!(second.contains("options vfio-pci disable_vga=1"));
        assert!(second.contains("# local comment"));
        assert!(second.contains("options vfio-pci ids=10de:2684"));
        assert!(second.contains("softdep nouveau pre: vfio-pci"));
        assert!(second.contains("softdep xhci_pci pre: vfio-pci"));
        assert!(second.contains("blacklist nouveau"));
        assert!(!second.contains("10de:22ba"));
        assert_eq!(second.matches(MANAGED_BEGIN).count(), 1);
        assert_eq!(second.matches(MANAGED_END).count(), 1);
    }

    #[test]
    fn detects_existing_unmanaged_vfio_ids_without_matching_managed_section() {
        assert!(has_unmanaged_vfio_ids("options vfio-pci ids=10de:2684\n"));
        assert!(!has_unmanaged_vfio_ids(
            "options vfio-pci disable_vga=1\n# local\n"
        ));
        assert!(!has_unmanaged_vfio_ids(&vfio_conf_with_ids(
            "",
            &["10de:2684".into()]
        )));
    }

    #[test]
    fn detects_vfio_ids_in_other_modprobe_files() {
        let root = std::env::temp_dir().join(format!("winship-modprobe-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let managed = root.join("vfio.conf");
        fs::write(root.join("local.conf"), "options vfio-pci ids=10de:2684\n").unwrap();
        assert!(has_external_vfio_ids(&root, &managed));
        fs::write(root.join("local.conf"), "options vfio-pci disable_vga=1\n").unwrap();
        assert!(!has_external_vfio_ids(&root, &managed));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn initramfs_modules_adds_required_modules_once() {
        let result = initramfs_modules_with_vfio("# existing\nvfio_pci\n");
        assert_eq!(result.lines().filter(|line| *line == "vfio_pci").count(), 1);
        for module in ["vfio", "vfio_iommu_type1"] {
            assert_eq!(result.lines().filter(|line| *line == module).count(), 1);
        }
    }

    #[test]
    fn rejects_disk_creation_outside_libvirt_image_directory() {
        let error = create_disk(Path::new("/tmp/winship.qcow2"), 80).unwrap_err();
        assert!(error.to_string().contains("/var/lib/libvirt/images"));
    }
}
