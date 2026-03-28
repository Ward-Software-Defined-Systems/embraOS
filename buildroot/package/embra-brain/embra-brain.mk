################################################################################
#
# embra-brain
#
################################################################################

EMBRA_BRAIN_VERSION = 0.2.0-phase1
EMBRA_BRAIN_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../../target/x86_64-unknown-linux-musl/release
EMBRA_BRAIN_SITE_METHOD = local

define EMBRA_BRAIN_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-brain $(TARGET_DIR)/usr/bin/embra-brain
endef

$(eval $(generic-package))
