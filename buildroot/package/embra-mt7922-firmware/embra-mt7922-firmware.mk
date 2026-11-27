################################################################################
#
# embra-mt7922-firmware
#
# The Wi-Fi firmware of MediaTek's MT7922 at a release of linux-firmware later
# than the one Buildroot's package carries.
#
# Buildroot 2026.02's linux-firmware is 20251011. The MT7922's firmware of
# that release (and of 20250917 before it; 20251021 went back to an earlier
# build of the file) carries a power table whose second record says it is
# sixteen bytes longer than the table. The kernel this image pins checks
# every record against the table (mt7921_load_clc: "Invalid CLC record", then
# "hardware init failed") and refuses the firmware, so a card that needs it
# is found, its driver is loaded, and no interface comes up. The firmware and
# its ROM patch are a pair of one release: both come from the release named
# below and are installed over the package's own, which is why this recipe
# depends on that package.
#
# The pin is the version below and the three sha256 lines of the hash file:
# the two files, and MediaTek's licence text of that release, which is the
# text the image carries already (post_build.sh installs the licences of
# linux-firmware's tree, and the image check compares the two). The files are
# fetched one by one from linux-firmware's own repository at the release's
# tag: the release's archive is 600 MB for 1.4 MB of files. Buildroot stops
# the build at a file whose sha256 is not the hash file's.
#
# scripts/lib/image-common.sh reads the two files' names and sha256 from the
# hash file and rejects an image whose /lib/firmware/mediatek does not hold
# exactly them, and walks every power table of the driver's family the way
# the driver does, whichever release a file is from
# (assert_mt7921_power_tables): keep the hash file's form.
#
# Before a move: walk the new file (the image check does, on the build's own
# target), and take the release's licence line with the files' when the text
# changed. This recipe goes when Buildroot's linux-firmware is 20251021 or
# later; Buildroot's master carries 20261019, the release pinned here.
#
################################################################################

EMBRA_MT7922_FIRMWARE_VERSION = 20261019
EMBRA_MT7922_FIRMWARE_TREE = https://gitlab.com/kernel-firmware/linux-firmware/-/raw/$(EMBRA_MT7922_FIRMWARE_VERSION)
EMBRA_MT7922_FIRMWARE_SITE = $(EMBRA_MT7922_FIRMWARE_TREE)/mediatek
EMBRA_MT7922_FIRMWARE_SOURCE = WIFI_RAM_CODE_MT7922_1.bin
EMBRA_MT7922_FIRMWARE_EXTRA_DOWNLOADS = \
	WIFI_MT7922_patch_mcu_1_1_hdr.bin \
	$(EMBRA_MT7922_FIRMWARE_TREE)/LICENSES/LICENCE.mediatek
EMBRA_MT7922_FIRMWARE_LICENSE = PROPRIETARY
EMBRA_MT7922_FIRMWARE_LICENSE_FILES = LICENCE.mediatek
EMBRA_MT7922_FIRMWARE_REDISTRIBUTE = NO
EMBRA_MT7922_FIRMWARE_DEPENDENCIES = linux-firmware

# The two files, as the driver asks for them under mediatek/.
EMBRA_MT7922_FIRMWARE_FILES = \
	WIFI_RAM_CODE_MT7922_1.bin \
	WIFI_MT7922_patch_mcu_1_1_hdr.bin

# No archive: the downloads are the files themselves.
define EMBRA_MT7922_FIRMWARE_EXTRACT_CMDS
	cp $(addprefix $(EMBRA_MT7922_FIRMWARE_DL_DIR)/,$(EMBRA_MT7922_FIRMWARE_FILES) LICENCE.mediatek) $(@D)/
endef

define EMBRA_MT7922_FIRMWARE_INSTALL_TARGET_CMDS
	$(foreach f,$(EMBRA_MT7922_FIRMWARE_FILES),\
		$(INSTALL) -D -m 0644 $(@D)/$(f) \
			$(TARGET_DIR)/lib/firmware/mediatek/$(f)$(sep))
endef

$(eval $(generic-package))
