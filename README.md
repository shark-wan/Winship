# Winship

Winship is a native Rust desktop manager for one Windows virtual machine on Debian 13 (Trixie), using libvirt/QEMU, Q35, OVMF, VFIO, and VirtIO Balloon. Windows video is sent to a monitor connected to the passed-through NVIDIA card. Winship does not provide remote display, RemoteApp, containers, clustering, or storage pools.

Winship is an early, hardware-dependent project. Review every device and IOMMU group before enabling VFIO. A device bound to VFIO is unavailable to Debian until the host is rebooted with a different driver configuration.

## Features

- Host checks for IOMMU boot flags, kernel IOMMU groups, NVIDIA display/audio functions, PCIe USB controllers, USB devices, removable disks, and OVMF firmware.
- A guided VFIO setup that writes `/etc/modprobe.d/vfio.conf`, adds the VFIO modules to initramfs-tools, and runs `update-initramfs -u -k all` through PolicyKit.
- A single fixed-name Windows VM with Q35/OVMF, installation and VirtIO driver ISOs, a qcow2 disk, configurable CPU and memory limits, and VirtIO Balloon.
- PCIe host devices, individually addressed USB devices, and physical whole-disk block passthrough.
- VM start, graceful shutdown, confirmed force stop, state, and memory reporting through `virsh`.
- Debian package metadata and a reproducible local `.deb` build script.

## Requirements

- Debian 13 (Trixie), x86_64, GRUB, and Secure Boot disabled.
- An IOMMU-capable host. The integrated GPU should remain with Debian; connect the monitor to the NVIDIA card used by the guest.
- `qemu-system-x86`, `qemu-utils`, `libvirt-daemon-system`, `libvirt-clients`, `ovmf`, `virtiofsd`, and a VirtIO driver ISO.
- `pkexec` and `policykit-1` for the explicit VFIO configuration action.
- A graphical X11 or Wayland session. The desktop environment's XDG portal is used for file selection.

Install the runtime packages and grant your desktop user access to libvirt:

```sh
sudo apt install qemu-system-x86 qemu-utils libvirt-daemon-system libvirt-clients ovmf pkexec polkitd xdg-desktop-portal initramfs-tools
sudo usermod -aG libvirt,kvm "$USER"
```

Log out and back in after changing group membership. Enable and start libvirt if the package did not do so:

```sh
sudo systemctl enable --now libvirtd
sudo virsh net-autostart default
sudo virsh net-info default
# If the network is inactive, run: sudo virsh net-start default
```

## Build and run

Install a Rust 1.85 or newer toolchain, then run:

```sh
cargo test --locked
cargo run --release
```

Build a Debian package on Debian 13:

```sh
./packaging/build-deb.sh
sudo apt install ./dist/winship_0.1.0_amd64.deb
```

The package installs a desktop entry and a PolicyKit action for the single VFIO setup helper. It does not change GRUB, bind devices, or create a VM during installation.

## First VM

1. Check **Host** and confirm IOMMU is active and Secure Boot is disabled. If IOMMU is not active, use the displayed Intel or AMD GRUB suggestion, update GRUB, and reboot before proceeding.
2. In **Virtual machine**, choose the Windows ISO, `virtio-win.iso`, a new qcow2 disk path, CPU count, startup memory, and maximum memory. Defaults are 4 GiB startup and 12 GiB maximum.
3. Select the NVIDIA display function and its audio function, plus the PCIe USB controller if desired. Keep every device in each selected IOMMU group available for passthrough; Winship lists group members and highlights incomplete groups. The VFIO `ids=` rule matches every host device with the selected vendor/device ID, so Winship blocks applying it if an unselected device has the same ID.
4. Apply the VFIO configuration from **Host**, then reboot. Winship intentionally does not reboot for you.
5. Define the VM and start it. Install Windows, load storage/network drivers from the VirtIO ISO when prompted, and install the VirtIO Balloon driver inside Windows.
6. Shut down Windows before editing passthrough devices. Starting the VM requires a monitor on the passed-through GPU output.

Physical disk passthrough uses `<disk type='block'>`. The entire selected disk and its partitions become unavailable to Debian while the VM runs. Unmount it first; do not use a disk containing host data. Choose a qcow2 path under `/var/lib/libvirt/images` so the system QEMU user can access the disk.

## Scope and safety

Winship manages one local Windows VM only. It does not support clusters, HA, Ceph, ZFS pools, LXC, network/firewall management, backups, RemoteApp, FreeRDP, or Looking Glass. PCI assignment can make the host unusable if the wrong device or IOMMU group is selected. Keep a recovery path and verify the selected BDFs against the physical hardware before applying VFIO.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

