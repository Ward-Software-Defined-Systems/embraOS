################################################################################
#
# embra-embedding-model
#
# Sentence-embedding model for embra-brain's semantic KG retrieval (KG-02).
# Architecture-INDEPENDENT data: unlike the binary packages this one must not
# reference $(EMBRAOS_RUST_TARGET) — one staged copy serves x86_64 and aarch64.
#
# The payload is staged host-side by scripts/build-image.sh (Step 3.6), which
# downloads it from Hugging Face and verifies its sha256 before staging — the
# same pin-then-trust discipline CI uses for the Bootlin SDK. Nothing large is
# committed to the repository.
#
# Installed read-only; embra-brain memory-maps nothing and reads it once at
# first use, so the rootfs copy stays shared and immutable.
#
################################################################################

EMBRA_EMBEDDING_MODEL_VERSION = 1.0
EMBRA_EMBEDDING_MODEL_SITE = $(BR2_EXTERNAL_EMBRAOS_PATH)/../vendor/embedding-model
EMBRA_EMBEDDING_MODEL_SITE_METHOD = local

# The installed directory name. Must match
# crate::embedding::EmbeddingProviderKind::default_model() and the directory
# staged by build-image.sh Step 3.6 — the brain resolves the model by this
# name under /usr/share/embra/models.
#
# NOT named EMBRA_EMBEDDING_MODEL_NAME: Buildroot's package infrastructure
# defines <PKG>_NAME itself (as the package name) and silently overrides any
# value set here, which installs the model to .../models/embra-embedding-model
# and leaves `/embeddings` reporting "model at: NOT FOUND".
EMBRA_EMBEDDING_MODEL_DIRNAME = bge-small-en-v1.5

define EMBRA_EMBEDDING_MODEL_INSTALL_TARGET_CMDS
	mkdir -p $(TARGET_DIR)/usr/share/embra/models/$(EMBRA_EMBEDDING_MODEL_DIRNAME)
	cp -a $(@D)/. $(TARGET_DIR)/usr/share/embra/models/$(EMBRA_EMBEDDING_MODEL_DIRNAME)/
endef

$(eval $(generic-package))
