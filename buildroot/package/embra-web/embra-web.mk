################################################################################
#
# embra-web
#
################################################################################

EMBRA_WEB_VERSION = 0.5.0-phase1
EMBRA_WEB_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/x86_64-unknown-linux-musl/release
EMBRA_WEB_SITE_METHOD = local

define EMBRA_WEB_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-web $(TARGET_DIR)/usr/bin/embra-web
endef

$(eval $(generic-package))
