################################################################################
#
# embra-console
#
################################################################################

EMBRA_CONSOLE_VERSION = 0.2.0-phase1
EMBRA_CONSOLE_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/x86_64-unknown-linux-musl/release
EMBRA_CONSOLE_SITE_METHOD = local

define EMBRA_CONSOLE_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-console $(TARGET_DIR)/usr/bin/embra-console
endef

$(eval $(generic-package))
