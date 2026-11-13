################################################################################
#
# embra-intel-microcode
#
# Intel's processor microcode at the release embraOS pins. Buildroot's own
# intel-microcode recipe names one release per Buildroot version (2026.02.1:
# 20260210-rev1) and Intel publishes several a year, so the bundle is this
# recipe's and Buildroot's package is not selected (the two would write the
# same images directory; Config.in says so).
#
# The pin is two lines that move together: the version below and the
# archive's sha256 in embra-intel-microcode.hash. The archive is GitHub's
# tarball of Intel's tag, the one Buildroot's recipe fetches, kept under the
# same name in the same download directory. Buildroot stops the build at an
# archive whose sha256 is not the hash file's and at one the hash file does
# not name. Before a move, compare the two bundles' intel-ucode directories
# and their licence, and take the licence's line with the archive's when it
# changed.
#
# scripts/lib/image-common.sh reads the version from the line below
# (intel_microcode_version) and rejects an image whose /boot/ucode.cpio or
# licence is not this bundle's (assert_boot_initrds): keep the line's form.
#
################################################################################

EMBRA_INTEL_MICROCODE_VERSION = 20261110
EMBRA_INTEL_MICROCODE_SITE = $(call github,intel,Intel-Linux-Processor-Microcode-Data-Files,microcode-$(EMBRA_INTEL_MICROCODE_VERSION))
EMBRA_INTEL_MICROCODE_SOURCE = intel-microcode-$(EMBRA_INTEL_MICROCODE_VERSION).tar.gz
EMBRA_INTEL_MICROCODE_DL_SUBDIR = intel-microcode
EMBRA_INTEL_MICROCODE_LICENSE = PROPRIETARY
EMBRA_INTEL_MICROCODE_LICENSE_FILES = license
EMBRA_INTEL_MICROCODE_REDISTRIBUTE = NO
EMBRA_INTEL_MICROCODE_INSTALL_IMAGES = YES

# The directory is written whole: post_build.sh packs every file in it into
# /boot/ucode.cpio, and a file Intel drops from a bundle must not ride on from
# the bundle before. The files stay off the rootfs: the kernel's early loader
# reads them from the first initrd and nothing loads microcode later.
define EMBRA_INTEL_MICROCODE_INSTALL_IMAGES_CMDS
	rm -rf $(BINARIES_DIR)/intel-ucode
	mkdir -p $(BINARIES_DIR)/intel-ucode
	$(INSTALL) -m 0644 -t $(BINARIES_DIR)/intel-ucode \
		$(@D)/intel-ucode/*
endef

# Intel's licence permits the binary redistribution on the condition that its
# notice travels with the distribution: the text goes on the rootfs, beside
# the firmware's licences, from the bundle the files come from.
define EMBRA_INTEL_MICROCODE_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0644 $(@D)/license \
		$(TARGET_DIR)/usr/share/licenses/intel-microcode/license
endef

# As Buildroot's recipe has them.
define EMBRA_INTEL_MICROCODE_LINUX_CONFIG_FIXUPS
	$(call KCONFIG_ENABLE_OPT,CONFIG_MICROCODE)
	$(call KCONFIG_ENABLE_OPT,CONFIG_MICROCODE_INTEL)
endef

$(eval $(generic-package))
