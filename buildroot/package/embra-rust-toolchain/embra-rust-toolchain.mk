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

define EMBRA_RUST_TOOLCHAIN_INSTALL_TARGET_CMDS
	mkdir -p $(TARGET_DIR)/opt/rust
	cp -a $(@D)/. $(TARGET_DIR)/opt/rust/
endef

$(eval $(generic-package))
