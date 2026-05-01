# syntax=docker/dockerfile:1.7
#
# Multi-stage: static musl binary → scratch (no base OS, just the binary).
# Builds natively for the host arch (no qemu / Rosetta overhead).

FROM rust:slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    musl-tools musl-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# rust-toolchain.toml drives nightly + components; we add the right musl
# target for THIS host's arch.
COPY rust-toolchain.toml ./
RUN rustup show && \
    case "$(uname -m)" in \
      x86_64)  TGT=x86_64-unknown-linux-musl  ;; \
      aarch64) TGT=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;; \
    esac && \
    rustup target add "$TGT" && \
    echo "$TGT" > /tgt

ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
    CC_aarch64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc

COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target,id=noyra-target \
    TGT=$(cat /tgt) && \
    cargo build --release --target "$TGT" && \
    cp "target/$TGT/release/noyra-sidecar" /noyra-sidecar && \
    strip /noyra-sidecar

FROM scratch
COPY --from=builder /noyra-sidecar /noyra-sidecar
USER 65532:65532
ENTRYPOINT ["/noyra-sidecar"]
