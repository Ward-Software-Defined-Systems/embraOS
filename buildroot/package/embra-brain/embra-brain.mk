################################################################################
#
# embra-brain
#
################################################################################

# Not the version of the software: the binary carries the workspace's
# (Cargo.toml). The value names the build directory and nothing else.
EMBRA_BRAIN_VERSION = 1.0
# The architecture comes from $(EMBRAOS_RUST_TARGET) (external.mk), never from
# a literal: one tree builds x86_64 and aarch64.
EMBRA_BRAIN_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/$(EMBRAOS_RUST_TARGET)/release
EMBRA_BRAIN_SITE_METHOD = local

define EMBRA_BRAIN_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embra-brain $(TARGET_DIR)/usr/bin/embra-brain
endef

$(eval $(generic-package))
