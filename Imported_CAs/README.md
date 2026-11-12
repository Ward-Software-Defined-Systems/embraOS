# Imported CAs

Certificate authorities of the builder's own, baked into an image at build time.

Drop a private CA's certificate into this folder before a build, and every machine installed from that build trusts it from its first boot: the raw disk image, the disks converted for VMware, Hyper-V and Parallels, a machine installed from the ISO, bare metal, and an update built from the same tree. The other route, `scripts/seed-state.sh --ca-dir`, writes to the STATE partition of a raw image on the build host and reaches none of the others.

## What goes here

- `*.pem` and `*.crt` files in PEM form, one certificate or several in a file.
- Certificates only. A file that carries a private key stops the build.
- Names of letters, digits, dots, dashes and underscores.

Nothing in this folder but this README is in git (`.gitignore`): every push of this repository is public, and a private CA's certificate names the hosts behind it. Each build host's checkout needs its own copy of the files.

## What the build does

- `scripts/build-image.sh` and `scripts/build-image-aarch64.sh` check the folder before anything is compiled (`imported_cas_check`, `scripts/lib/imported-cas.sh`) and list what will be baked: each certificate's subject, SHA-256 fingerprint and expiry, with a note on one that is not a CA certificate or has expired.
- The build stops on a file that is not a PEM certificate file, that carries a private key or another kind of PEM block, that is larger than 256 KiB, whose name is outside the grammar above, or that is of another kind (a DER `.cer`, a `.key`, a folder). This README and dotfiles are left alone.
- `buildroot/board/embraos/post_build.sh` copies the files into the root filesystem, at `/usr/share/embra/ca-certificates/`.
- After Buildroot the build checks that the image holds exactly the folder's files, byte for byte (`assert_imported_cas`, `scripts/lib/image-common.sh`).

## What the OS does

- embrad merges the image's certificates and those of `/embra/state/ca-certificates/` with the stock bundle at every boot and exports `SSL_CERT_FILE` and `GIT_SSL_CAINFO` to every service (`crates/embrad/src/ca_bundle.rs`).
- The brain's HTTP clients trust both directories beside the compiled-in roots (`crates/embra-brain/src/net.rs`): the providers through an HTTPS gateway, the GitLab and GitHub tools, the Guardian transport of `http_get` and `http_request`. One client does not: `web_search`'s, which reaches `api.search.brave.com` on the compiled-in roots alone.

## What to know

- The trust follows the image. A machine updated to an image built without a file no longer trusts that CA. A CA that has to outlive updates belongs in STATE's `ca-certificates/`, which `seed-state.sh --ca-dir` fills on a raw image; an installed machine has no step for that yet.
- Every machine installed from the build trusts these CAs on the connections named above, for any host name. Bake only CAs you operate.
- `scripts/release-update.sh` signs no release from an image that carries imported CAs, nor from a tree whose folder holds certificate files. `--allow-imported-cas` overrides both.
- To remove a CA, take its file out of the folder and rebuild.
