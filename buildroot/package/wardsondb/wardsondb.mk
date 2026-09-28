################################################################################
#
# wardsondb
#
################################################################################

WARDSONDB_VERSION = 0.1.0
# The architecture comes from $(EMBRAOS_RUST_TARGET) (external.mk), never from
# a literal: one tree builds x86_64 and aarch64.
WARDSONDB_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/$(EMBRAOS_RUST_TARGET)/release
WARDSONDB_SITE_METHOD = local

define WARDSONDB_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/wardsondb $(TARGET_DIR)/usr/bin/wardsondb
endef

$(eval $(generic-package))
