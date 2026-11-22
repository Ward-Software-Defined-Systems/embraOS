################################################################################
#
# embra-llama-cpp
#
# The model server of native models (experimental): llama.cpp's llama-server.
# Buildroot's own llama-cpp recipe names an older build (2026.02.1: b7271)
# and has no switch for the CPU backends as libraries, so the server is this
# recipe's and Buildroot's package is not selected (Config.in says so).
#
# The pin is two lines that move together: the version below and the
# archive's sha256 in embra-llama-cpp.hash. The archive is GitHub's tarball
# of the tag. Buildroot stops the build at an archive whose sha256 is not the
# hash file's. scripts/lib/image-common.sh reads the version from the line
# below (llama_cpp_version) for the checks of the root filesystem
# (assert_native_model_runtime): keep the line's form. A change of this
# directory builds the package again from its tarball
# (clean_package_with_changed_recipe): Buildroot rebuilds nothing whose
# options changed.
#
# What is built, and why:
# - the server alone (the target llama-server): no command-line tool, no web
#   page (LLAMA_BUILD_UI and LLAMA_USE_PREBUILT_UI, which would fetch one),
#   no TLS (LLAMA_OPENSSL), no child processes (LLAMA_SUBPROCESS) and no
#   download code. It listens on loopback and reads one file embraOS checked;
# - the CPU backends as libraries the server loads at start (GGML_BACKEND_DL,
#   GGML_CPU_ALL_VARIANTS): one for each instruction set, down to a baseline
#   that needs none beyond the architecture's, since some machines embraOS
#   runs on have no AVX. GGML_NATIVE off: the build host's processor decides
#   nothing;
# - threads of its own, no OpenMP runtime; no BLAS; no Vulkan in this step.
#
# The compiler is handed the stand-in for Buildroot's directory
# (EMBRAOS_BUILD_DIR_MAP, external.mk): every file of llama.cpp names its
# source through __FILE__ in an assertion. Buildroot hands a cmake package
# its flags through the toolchain file, which stands back when the command
# line names them, so the two lines carry the target's own flags in front.
# No run path is written: a build tree's would name that directory, and
# /usr/lib is where the libraries are.
#
################################################################################

EMBRA_LLAMA_CPP_VERSION = 1.0.0
EMBRA_LLAMA_CPP_SITE = https://github.com/ggml-org/llama.cpp/archive/refs/tags
EMBRA_LLAMA_CPP_SOURCE = v$(EMBRA_LLAMA_CPP_VERSION).tar.gz
EMBRA_LLAMA_CPP_LICENSE = MIT, BSD-2-Clause (xxhash)
EMBRA_LLAMA_CPP_LICENSE_FILES = \
	LICENSE \
	vendor/cpp-httplib/LICENSE \
	licenses/LICENSE-jsonhpp \
	vendor/hash/xxhash/LICENSE \
	vendor/hash/rotate-bits/LICENSE.md
EMBRA_LLAMA_CPP_CPE_ID_VENDOR = ggml
EMBRA_LLAMA_CPP_CPE_ID_PRODUCT = llama.cpp
EMBRA_LLAMA_CPP_SUPPORTS_IN_SOURCE_BUILD = NO

EMBRA_LLAMA_CPP_CONF_OPTS = \
	-DCMAKE_C_FLAGS="$(TARGET_CFLAGS) $(EMBRAOS_BUILD_DIR_MAP)" \
	-DCMAKE_CXX_FLAGS="$(TARGET_CXXFLAGS) $(EMBRAOS_BUILD_DIR_MAP)" \
	-DCMAKE_SKIP_RPATH=ON \
	-DGGML_BACKEND_DL=ON \
	-DGGML_CPU_ALL_VARIANTS=ON \
	-DGGML_NATIVE=OFF \
	-DGGML_BACKEND_DIR=/usr/lib/llama-cpp \
	-DGGML_OPENMP=OFF \
	-DGGML_CCACHE=OFF \
	-DGGML_VULKAN=OFF \
	-DGGML_BLAS=OFF \
	-DLLAMA_BUILD_SERVER=ON \
	-DLLAMA_BUILD_TOOLS=ON \
	-DLLAMA_BUILD_TESTS=OFF \
	-DLLAMA_BUILD_EXAMPLES=OFF \
	-DLLAMA_BUILD_APP=OFF \
	-DLLAMA_BUILD_UI=OFF \
	-DLLAMA_USE_PREBUILT_UI=OFF \
	-DLLAMA_OPENSSL=OFF \
	-DLLAMA_SUBPROCESS=OFF \
	-DLLAMA_CONNECT=OFF \
	-DLLAMA_BUILD_IS_DEV=OFF

# The server's target and what it links, nothing else of the tree (the
# server lives under tools/, so LLAMA_BUILD_TOOLS is on above).
EMBRA_LLAMA_CPP_BUILD_OPTS = --target llama-server

EMBRA_LLAMA_CPP_BIN = $(@D)/buildroot-build/bin
EMBRA_LLAMA_CPP_LICENSES = $(TARGET_DIR)/usr/share/licenses/llama.cpp

# The files the server runs on and no other: the launcher, the libraries it
# is linked against (each with the link its soname names, never the
# unversioned one a build links through), and the CPU backends. cmake's own
# install wants every target built and brings headers and build files. The
# backends' directory is written whole: a backend a later release drops must
# not ride on from the release before. The licence texts travel with the
# binaries: llama.cpp's, and those of the code it vendors that ask for it.
define EMBRA_LLAMA_CPP_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(EMBRA_LLAMA_CPP_BIN)/llama-server $(TARGET_DIR)/usr/bin/llama-server
	$(INSTALL) -D -m 0755 $(EMBRA_LLAMA_CPP_BIN)/libllama-server-impl.so $(TARGET_DIR)/usr/lib/libllama-server-impl.so
	for lib in ggml-base ggml llama llama-common mtmd; do \
		cp -a $(EMBRA_LLAMA_CPP_BIN)/lib$$lib.so.* $(TARGET_DIR)/usr/lib/ || exit 1; \
	done
	rm -rf $(TARGET_DIR)/usr/lib/llama-cpp
	mkdir -p $(TARGET_DIR)/usr/lib/llama-cpp
	$(INSTALL) -m 0755 -t $(TARGET_DIR)/usr/lib/llama-cpp $(EMBRA_LLAMA_CPP_BIN)/libggml-cpu-*.so
	rm -rf $(EMBRA_LLAMA_CPP_LICENSES)
	$(INSTALL) -D -m 0644 $(@D)/LICENSE $(EMBRA_LLAMA_CPP_LICENSES)/LICENSE
	$(INSTALL) -D -m 0644 $(@D)/vendor/cpp-httplib/LICENSE $(EMBRA_LLAMA_CPP_LICENSES)/LICENSE-cpp-httplib
	$(INSTALL) -D -m 0644 $(@D)/licenses/LICENSE-jsonhpp $(EMBRA_LLAMA_CPP_LICENSES)/LICENSE-nlohmann-json
	$(INSTALL) -D -m 0644 $(@D)/vendor/hash/xxhash/LICENSE $(EMBRA_LLAMA_CPP_LICENSES)/LICENSE-xxhash
	$(INSTALL) -D -m 0644 $(@D)/vendor/hash/rotate-bits/LICENSE.md $(EMBRA_LLAMA_CPP_LICENSES)/LICENSE-rotate-bits
endef

$(eval $(cmake-package))
