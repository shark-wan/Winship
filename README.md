# Winship

Winship is a local web manager for one Windows virtual machine on Debian testing (sid). It uses libvirt/QEMU with Q35, OVMF/UEFI, VFIO passthrough, and VirtIO Balloon. Windows video goes to a monitor connected to the passed-through NVIDIA card; Winship does not stream the guest desktop back to Linux.

The service is written in Rust with axum and Askama. Templates, CSS, and JavaScript are compiled into the `winship` binary. Access is protected by a locally generated token.

## Features

- Host readiness checks for IOMMU, Secure Boot, OVMF, PCI/IOMMU groups, USB devices, removable disks, and network interfaces.
- Guided VFIO setup: writes `/etc/modprobe.d/vfio.conf`, adds VFIO modules to `/etc/initramfs-tools/modules`, and runs `update-initramfs -u -k all` through the PolicyKit helper.
- One fixed-name VM (`winship-windows`) with Q35 + OVMF, Windows and VirtIO ISOs, a qcow2 system disk, configurable CPU and memory, and VirtIO Balloon.
- NVIDIA display/audio PCI passthrough, whole USB-controller passthrough, individual USB-device passthrough, and whole physical block-disk passthrough.
- NAT by default, wired-only bridge mode, and an explicitly marked experimental macvtap mode.
- VM start, ACPI shutdown, confirmed force stop, live state, and memory/CPU reporting.
- Token authentication on every page and API, plus SameSite/HttpOnly session cookies.
- Debian package with a systemd user service that runs the web process as an unprivileged desktop user.

## Architecture

```text
browser ── HTTP + token ──> winship (axum, ordinary user)
                              ├── host.rs       sysfs, PCI, USB, disks, OVMF
                              ├── libvirt.rs    virsh qemu:///system
                              ├── model.rs      config validation and domain data
                              ├── privileged.rs pkexec helper for root operations
                              └── web/          routes, auth, Askama views
```

The VM is always `qemu:///system`. The unprivileged service user must be in the `libvirt` group. Root-only VFIO and media/disk operations still use the installed PolicyKit helper; the web service never runs as root.

## Requirements

- Debian testing/sid, x86_64, UEFI/GRUB, Secure Boot disabled.
- IOMMU enabled. The integrated GPU should remain with Debian. Connect the monitor to the NVIDIA card passed to Windows.
- `qemu-system-x86`, `qemu-utils`, `libvirt-daemon-system`, `libvirt-clients`, `ovmf`, `pkexec`, `polkitd`, and `initramfs-tools`.
- A VirtIO driver ISO (`virtio-win.iso`) and a Windows installation ISO.
- A graphical PolicyKit agent for interactive privileged operations when running without a terminal.

Install the runtime manually when running from source:

```sh
sudo apt install qemu-system-x86 qemu-utils libvirt-daemon-system libvirt-clients \
  ovmf pkexec polkitd initramfs-tools xdg-utils
sudo usermod -aG libvirt,kvm "$USER"
sudo systemctl enable --now libvirtd
sudo virsh net-autostart default
sudo virsh net-start default
```

Log out and back in after changing group membership.

## Build and test

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

For local development, override the production paths and listen on loopback:

```sh
WINSHIP_LISTEN=127.0.0.1:15277 \
WINSHIP_TOKEN_FILE=/tmp/winship-token \
WINSHIP_CONFIG_FILE=/tmp/winship-vm.json \
cargo run
```

The first process start creates the token with mode `0600`. Open `http://127.0.0.1:15277/`, enter the token from the token file, and continue to the UI.

The production default is `0.0.0.0:15277`, token `/etc/winship/token`, and config `/var/lib/winship/vm.json` when launched by the packaged service.

## Debian package

Build a local package:

```sh
./packaging/build-deb.sh
sudo apt install ./dist/winship_0.1.0_amd64.deb
```

The package:

- runs `winship.service` as the desktop user who invoked `sudo apt install`, or creates a `winship` system user when installed by root directly, and adds it to `libvirt` and `kvm`;
- enables the systemd user service and lingering so it starts at boot; it can also be started on demand from the desktop launcher;
- creates `/etc/winship` with mode `0700`; the web process generates `/etc/winship/token` on its first start with mode `0600`;
- installs a desktop launcher that opens the local web UI;
- installs the PolicyKit action for VFIO/initramfs operations;
- configures the Plasma session to bind KWin to the integrated GPU only (applies at the next Plasma login).

Read the token and manage the service:

```sh
sudo cat /etc/winship/token
systemctl --user status winship.service
systemctl --user restart winship.service
```

Interactive VFIO and disk/media operations use the KDE PolicyKit agent. They require an active graphical login for the service user; if Winship starts at boot, wait until that user is logged in before triggering privileged actions.

Open `http://<host>:15277/` from a trusted machine. Because the service listens on all interfaces, use a firewall or set `WINSHIP_LISTEN=127.0.0.1:15277` in a service override if remote access is not needed.

## First VM

1. Open **硬件与直通** and confirm IOMMU is active, Secure Boot is off, and OVMF firmware is present.
2. Open **虚拟机** and select the NVIDIA display/audio functions and the USB controller. Every member of a selected IOMMU group must be selected.
3. Apply VFIO from the host page. Review the group warning, authenticate through PolicyKit, and reboot the host. Winship intentionally does not reboot for you.
4. Select or import the Windows ISO and `virtio-win.iso`, create a qcow2 disk under `/var/lib/libvirt/images`, set CPU/memory, and save the definition.
5. Start Windows, install storage/network drivers from the VirtIO ISO when prompted, then install the VirtIO Balloon driver inside Windows.
6. After installation, turn off **优先从安装 ISO 启动** and update the definition.

Physical disk passthrough uses `<disk type='block'>`. The selected whole disk and all its partitions become unavailable to Debian while the VM runs. Unmount it first; never choose a disk containing host data.

## Security notes

- The HTTP UI uses a bearer token; it is not TLS. Use it on a trusted local network only.
- All state-changing requests are POSTs. The token cookie is HttpOnly and SameSite=Strict.
- Device passthrough mistakes can leave the host without display or input. Verify every BDF and IOMMU group and keep an out-of-band recovery path.
- The service process is unprivileged; privileged operations are limited to the installed PolicyKit helper commands. The user service deliberately avoids mount/seccomp sandboxing so the setuid `pkexec` helper can work; protect the token and listen address accordingly.

## Explicit non-goals

Winship does not implement clusters, HA, Ceph, ZFS pools, LXC, backup servers, SDN, firewalls, Docker/Podman VM execution, RemoteApp, FreeRDP composition, Looking Glass, or cross-platform support.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
