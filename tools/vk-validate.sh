#!/bin/sh
# Runs a command under the Khronos validation layer, with synchronization
# validation, and fails if the layer reports anything not in the allowlist.
#
#   tools/vk-validate.sh cargo test --workspace -- --test-threads=1
#   tools/vk-validate.sh cargo run -- clip.mp4
#
# The layer is injected through the loader (VK_INSTANCE_LAYERS) rather than
# FFmpeg's `debug` option, for two reasons:
# - FFmpeg's debug callback drops some messages (VUID-VkImageCreateInfo-pNext-06811
#   and VUID-VkImageViewCreateInfo-usage-02275 never reached the log).
# - Handle wrapping (the layer's `unique_handles`) crashes the NVIDIA driver in
#   FFmpeg's Vulkan Video decode, so it is turned off here.
#
# Synchronization validation in layer 1.4.341 does not see shader accesses
# through descriptors (storage images and buffers, sampled images): it checks
# layout transitions, transfers and submissions only. Compute barriers need
# review by hand.
#
# Needs `sudo apt install vulkan-validationlayers`. The layer prints to
# stdout; the command's output is also saved to $VK_VALIDATE_LOG (default:
# target/vk-validate.log).

set -u
root=$(cd "$(dirname "$0")/.." && pwd)
log=${VK_VALIDATE_LOG:-$root/target/vk-validate.log}
mkdir -p "$(dirname "$log")"

settings=$(mktemp -d)
trap 'rm -rf "$settings"' EXIT
cat >"$settings/vk_layer_settings.txt" <<EOF
khronos_validation.validate_sync = true
khronos_validation.unique_handles = false
khronos_validation.debug_action = VK_DBG_LAYER_ACTION_LOG_MSG
khronos_validation.report_flags = error,warn
khronos_validation.log_filename = stdout
EOF

# One log for every process the command starts (each test binary is one).
{
    VK_LAYER_SETTINGS_PATH="$settings/vk_layer_settings.txt" \
    VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation \
        "$@" 2>&1
    echo $? >"$settings/status"
} | tee "$log"
status=$(cat "$settings/status")

# Known issues in FFmpeg 8.0 (plain `ffmpeg` reports the same ones):
# - decode/encode images are created VIDEO_PROFILE_INDEPENDENT with usage or
#   flags no video format property lists (06811, 07142, 07240, 08206)
# - views of whole multi-planar images inherit STORAGE usage (02275)
# - the Vulkan encoder leaks image views at teardown (05137)
allowed='VUID-VkImageCreateInfo-pNext-06811
VUID-vkCmdDecodeVideoKHR-pDecodeInfo-07142
VUID-VkVideoBeginCodingInfoKHR-pPictureResource-07240
VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08206
VUID-VkImageViewCreateInfo-usage-02275
VUID-vkDestroyDevice-device-05137'

ids=$(grep -oE '\[ [A-Za-z0-9_-]+ \]' "$log" | tr -d '[] ' | sort | uniq -c)
unexpected=$(printf '%s\n' "$ids" | while read -r count id; do
    [ -n "${id:-}" ] || continue
    printf '%s\n' "$allowed" | grep -q -x -F "$id" || echo "$count $id"
done)

echo "vk-validate: log in $log"
if [ -n "$ids" ]; then
    echo "vk-validate: messages by id:"
    printf '%s\n' "$ids"
fi
if [ -n "$unexpected" ]; then
    echo "vk-validate: unexpected messages:"
    printf '%s\n' "$unexpected"
    exit 1
fi
exit $status
