################################################################################
#
# embra-web
#
################################################################################

# Not the version of the software: the binary carries the workspace's
# (Cargo.toml). The value names the build directory and nothing else.
EMBRA_WEB_VERSION = 1.0
# The architecture comes from $(EMBRAOS_RUST_TARGET) (external.mk), never from
# a literal: one tree builds x86_64 and aarch64.
EMBRA_WEB_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/$(EMBRAOS_RUST_TARGET)/release
EMBRA_WEB_SITE_METHOD = local

define EMBRA_WEB_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-web $(TARGET_DIR)/usr/bin/embra-web
endef

$(eval $(generic-package))
