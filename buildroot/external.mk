# Rust target triple for the musl-static embraOS binaries, selected by the
# Buildroot architecture (set via embraos_x86_64_defconfig /
# embraos_aarch64_defconfig). Each package .mk uses $(EMBRAOS_RUST_TARGET)
# in its _SITE, so one committed tree builds both arches with no per-arch
# sed of the package makefiles.
EMBRAOS_RUST_TARGET = $(if $(BR2_aarch64),aarch64-unknown-linux-musl,x86_64-unknown-linux-musl)

include $(sort $(wildcard $(BR2_EXTERNAL_EMBRAOS_PATH)/package/*/*.mk))
