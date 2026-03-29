################################################################################
#
# embrad
#
################################################################################

EMBRAD_VERSION = 0.2.0-phase1
EMBRAD_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../target/x86_64-unknown-linux-musl/release
EMBRAD_SITE_METHOD = local

define EMBRAD_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/embrad $(TARGET_DIR)/sbin/embrad
endef

$(eval $(generic-package))
