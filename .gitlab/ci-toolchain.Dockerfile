# CI toolchain image for the Linux test jobs: Rust 1.97.1 with clippy and
# rustfmt, and shellcheck for the fetch-miners.sh lint.
#
# The `build-toolchain-image` job in `.gitlab-ci.yml` builds and pushes this
# image. It runs automatically on a branch push that changes this file and is
# manual otherwise. The jobs pull the image by digest, so a rebuild takes
# effect only when a commit updates TOOLCHAIN_IMAGE.

# Nothing watches this pin for updates. Check the rust image tags on a rebuild,
# and keep the version equal to the channel in rust-toolchain.toml.
FROM rust:1.97.1-trixie@sha256:b1b3c9c0d921d7fa0a6d1f9ec7e4eab87f8c8ec97644c3d791450f131dec813f

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# rust-toolchain.toml names these components, so without them rustup downloads
# both at the start of every job.
RUN rustup component add clippy rustfmt

# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends shellcheck \
    && rm -rf /var/lib/apt/lists/*

# Fail the image build here, not in a CI job.
RUN cargo --version && cargo clippy --version && cargo fmt --version \
    && shellcheck --version | head -2 \
    && command -v curl wget
