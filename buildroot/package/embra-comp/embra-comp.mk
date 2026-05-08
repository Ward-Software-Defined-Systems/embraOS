################################################################################
#
# embra-comp
#
################################################################################

EMBRA_COMP_VERSION = 0.5.0-phase1
EMBRA_COMP_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/x86_64-unknown-linux-musl/release
EMBRA_COMP_SITE_METHOD = local

# embra-comp depends on the same userspace libs the kernel exposes for a
# Wayland session: libdrm/gbm for KMS scanout, libinput for input, libseat
# for session take-over, libudev for hot-plug, mesa3d for the EGL/GLES
# context behind the renderer.
EMBRA_COMP_DEPENDENCIES = libdrm libinput libseat libxkbcommon mesa3d wayland

define EMBRA_COMP_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-comp $(TARGET_DIR)/sbin/embra-comp
endef

$(eval $(generic-package))
