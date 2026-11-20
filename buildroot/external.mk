# Rust target triple for the musl-static embraOS binaries, selected by the
# Buildroot architecture (set via embraos_x86_64_defconfig /
# embraos_aarch64_defconfig). Each package .mk uses $(EMBRAOS_RUST_TARGET)
# in its _SITE, so one committed tree builds both arches with no per-arch
# sed of the package makefiles.
EMBRAOS_RUST_TARGET = $(if $(BR2_aarch64),aarch64-unknown-linux-musl,x86_64-unknown-linux-musl)

include $(sort $(wildcard $(BR2_EXTERNAL_EMBRAOS_PATH)/package/*/*.mk))

# The build directory stays out of what three packages compile it into
# (scripts/lib/build-paths.sh has the rest of this). sqlite's shell and
# WebKit name their own sources through __FILE__, and on a Linux host
# Buildroot's directory lies under the builder's home: the compiler is handed
# a stand-in for it. Buildroot hands a cmake package its flags through the
# toolchain file, which stands back when the command line names them, so
# WebKit's two lines carry the target's own flags in front. libpsl's table
# names the list it was made from: buildroot/patches/libpsl.
EMBRAOS_BUILD_DIR_MAP = -ffile-prefix-map=$(BASE_DIR)=/buildroot
SQLITE_CFLAGS += $(EMBRAOS_BUILD_DIR_MAP)
WPEWEBKIT_CONF_OPTS += \
	-DCMAKE_C_FLAGS="$(TARGET_CFLAGS) $(EMBRAOS_BUILD_DIR_MAP)" \
	-DCMAKE_CXX_FLAGS="$(TARGET_CXXFLAGS) $(EMBRAOS_BUILD_DIR_MAP)"

# The kernel's banner (the first line of its log, /proc/version) names who
# built it: the login that ran the build and the host's name, unless its
# build is told another (scripts/mkcompile_h in the kernel's tree). An image
# names neither. The name is KERNEL_BUILT_BY in scripts/lib/build-paths.sh,
# which reads the built kernel back.
LINUX_MAKE_ENV += KBUILD_BUILD_USER=embraos KBUILD_BUILD_HOST=embraos
