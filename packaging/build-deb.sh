#!/bin/sh
set -eu

root_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=0.1.0
architecture=$(dpkg --print-architecture)
if [ "$architecture" != amd64 ]; then
    printf 'Winship packages are supported on amd64; found %s\n' "$architecture" >&2
    exit 1
fi

cd "$root_dir"
cargo build --release --locked

stage=$(mktemp -d "${TMPDIR:-/tmp}/winship-deb.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT HUP INT TERM
install -D -m 0755 target/release/winship "$stage/usr/bin/winship"
install -D -m 0755 packaging/winship-launch "$stage/usr/bin/winship-launch"
install -D -m 0644 packaging/winship.service "$stage/usr/lib/systemd/user/winship.service"
install -D -m 0644 packaging/winship.desktop "$stage/usr/share/applications/winship.desktop"
install -D -m 0644 packaging/org.winship.apply-vfio.policy "$stage/usr/share/polkit-1/actions/org.winship.apply-vfio.policy"
install -D -m 0644 packaging/90-winship-igpu.sh "$stage/etc/xdg/plasma-workspace/env/90-winship-igpu.sh"
install -D -m 0644 packaging/90-winship-igpu.conf "$stage/etc/environment.d/90-winship-igpu.conf"
install -D -m 0755 debian/postinst "$stage/DEBIAN/postinst"
install -D -m 0755 debian/prerm "$stage/DEBIAN/prerm"
install -D -m 0644 debian/control "$stage/DEBIAN/control"
install -D -m 0644 debian/copyright "$stage/usr/share/doc/winship/copyright"
install -D -m 0644 README.md "$stage/usr/share/doc/winship/README.md"
install -D -m 0644 LICENSE "$stage/usr/share/doc/winship/LICENSE"
chmod 0755 "$stage"

mkdir -p dist
dpkg-deb --build --root-owner-group "$stage" "dist/winship_${version}_${architecture}.deb"
printf 'Created %s/dist/winship_%s_%s.deb\n' "$root_dir" "$version" "$architecture"
