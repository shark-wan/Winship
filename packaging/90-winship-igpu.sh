# Keep the Plasma/KWin compositor on a non-NVIDIA DRM device.
# The NVIDIA card is reserved for VFIO passthrough to Windows.
for card in /sys/class/drm/card[0-9]*; do
    [ -e "$card/device/vendor" ] || continue
    vendor=$(cat "$card/device/vendor" 2>/dev/null || true)
    [ "$vendor" = "0x10de" ] && continue
    candidate="/dev/dri/$(basename "$card")"
    if [ -e "$candidate" ]; then
        export KWIN_DRM_DEVICES="$candidate"
        break
    fi
done
