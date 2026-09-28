################################################################################
#
# embra-trustd
#
################################################################################

EMBRA_TRUSTD_VERSION = 0.2.0-phase1
# The architecture comes from $(EMBRAOS_RUST_TARGET) (external.mk), never from
# a literal: one tree builds x86_64 and aarch64.
EMBRA_TRUSTD_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/$(EMBRAOS_RUST_TARGET)/release
EMBRA_TRUSTD_SITE_METHOD = local

define EMBRA_TRUSTD_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-trustd $(TARGET_DIR)/usr/bin/embra-trustd
endef

$(eval $(generic-package))
