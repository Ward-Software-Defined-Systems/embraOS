################################################################################
#
# embrad
#
################################################################################

# Not the version of the software: the binary carries the workspace's
# (Cargo.toml). The value names the build directory and nothing else.
EMBRAD_VERSION = 1.0
# The architecture comes from $(EMBRAOS_RUST_TARGET) (external.mk), never from
# a literal: one tree builds x86_64 and aarch64.
EMBRAD_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/$(EMBRAOS_RUST_TARGET)/release
EMBRAD_SITE_METHOD = local

define EMBRAD_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embrad $(TARGET_DIR)/sbin/embrad
endef

$(eval $(generic-package))
