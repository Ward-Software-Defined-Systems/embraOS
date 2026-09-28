################################################################################
#
# embra-apid
#
################################################################################

EMBRA_APID_VERSION = 0.2.0-phase1
# The architecture comes from $(EMBRAOS_RUST_TARGET) (external.mk), never from
# a literal: one tree builds x86_64 and aarch64.
EMBRA_APID_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/$(EMBRAOS_RUST_TARGET)/release
EMBRA_APID_SITE_METHOD = local

define EMBRA_APID_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-apid $(TARGET_DIR)/usr/bin/embra-apid
endef

$(eval $(generic-package))
