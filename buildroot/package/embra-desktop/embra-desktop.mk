################################################################################
#
# embra-desktop
#
################################################################################

EMBRA_DESKTOP_VERSION = 0.5.0-phase1
EMBRA_DESKTOP_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/x86_64-unknown-linux-musl/release
EMBRA_DESKTOP_SITE_METHOD = local

# Userspace deps the Wayland client needs at runtime: libwayland-client +
# xkbcommon for input, fontconfig + freetype + harfbuzz + DejaVu for
# typography, libxkbcommon-dev provides keyboard layout tables, mesa3d
# carries softbuffer/EGL touchpoints if iced ever needs GPU.
EMBRA_DESKTOP_DEPENDENCIES = wayland libxkbcommon fontconfig freetype harfbuzz dejavu mesa3d

define EMBRA_DESKTOP_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-desktop $(TARGET_DIR)/usr/bin/embra-desktop
endef

$(eval $(generic-package))
