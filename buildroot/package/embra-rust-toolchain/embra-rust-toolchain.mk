################################################################################
#
# embra-rust-toolchain
#
# Prebuilt Rust toolchain (musl host + wasm32 std + rust-lld) for
# embra-guardian-v1. The relocatable prefix is staged host-side by
# scripts/build-image.sh (Step 3.5) into vendor/rust-toolchain and
# installed read-only at /opt/rust. rustc finds its sysroot relative to
# its own binary, so the tree is position-independent under the prefix.
#
################################################################################

EMBRA_RUST_TOOLCHAIN_VERSION = 1.0
EMBRA_RUST_TOOLCHAIN_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../vendor/rust-toolchain
EMBRA_RUST_TOOLCHAIN_SITE_METHOD = local

# The prefix is REPLACED, never merged into. Buildroot's target dir persists
# across builds and rustc names its libraries by hash
# (librustc_driver-<hash>.so, libcore-<hash>.rlib), so after a toolchain
# version change a plain copy would ship the old compiler's libraries next
# to the new one's.
#
# Only the payload is installed. $(@D) is Buildroot's build directory for the
# package, and Buildroot keeps its own bookkeeping at the top of it
# (.stamp_*, .files-list*). The toolchain has no dotfile of its own there.
define EMBRA_RUST_TOOLCHAIN_INSTALL_TARGET_CMDS
	rm -rf $(TARGET_DIR)/opt/rust
	mkdir -p $(TARGET_DIR)/opt/rust
	cp -a $(@D)/. $(TARGET_DIR)/opt/rust/
	find $(TARGET_DIR)/opt/rust -maxdepth 1 -type f -name '.*' -delete
endef

$(eval $(generic-package))
