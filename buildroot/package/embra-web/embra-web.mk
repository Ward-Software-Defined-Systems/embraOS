################################################################################
#
# embra-web
#
################################################################################

EMBRA_WEB_VERSION = 0.5.0-phase1
# The architecture comes from $(EMBRAOS_RUST_TARGET) (external.mk), never from
# a literal: one tree builds x86_64 and aarch64.
EMBRA_WEB_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/$(EMBRAOS_RUST_TARGET)/release
EMBRA_WEB_SITE_METHOD = local

define EMBRA_WEB_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-web $(TARGET_DIR)/usr/bin/embra-web
endef

$(eval $(generic-package))
