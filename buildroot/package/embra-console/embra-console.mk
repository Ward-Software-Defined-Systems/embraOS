################################################################################
#
# embra-console
#
################################################################################

EMBRA_CONSOLE_VERSION = 0.2.0-phase1
# The architecture comes from $(EMBRAOS_RUST_TARGET) (external.mk), never from
# a literal: one tree builds x86_64 and aarch64.
EMBRA_CONSOLE_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/$(EMBRAOS_RUST_TARGET)/release
EMBRA_CONSOLE_SITE_METHOD = local

define EMBRA_CONSOLE_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-console $(TARGET_DIR)/usr/bin/embra-console
endef

$(eval $(generic-package))
