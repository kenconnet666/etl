FROM debian:trixie-slim

ARG TARGETARCH=amd64
ARG RUSTFS_VERSION=1.0.0-beta.8

RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        curl ca-certificates unzip \
    && rm -rf /var/lib/apt/lists/*

RUN case "$TARGETARCH" in \
        amd64)  RFS_ARCH=x86_64-gnu;  RFS_SHA=f6dbe05430743820db5c38efda403bfa7c1b30967013f792735ab37cee1dc137; MC_ARCH=amd64 ;; \
        arm64)  RFS_ARCH=aarch64-gnu; RFS_SHA=8709ec7eb7a0cdc875d1d1bc1d5ee2b17a801ea89bb8ce4a6b15443cf24c0cf3; MC_ARCH=arm64 ;; \
        *) echo "unsupported arch: $TARGETARCH" && exit 1 ;; \
    esac \
    && curl -fsSL "https://dl.rustfs.com/artifacts/rustfs/release/rustfs-linux-${RFS_ARCH}-v${RUSTFS_VERSION}.zip" -o /tmp/rustfs.zip \
    && echo "${RFS_SHA}  /tmp/rustfs.zip" | sha256sum -c - \
    && unzip -q /tmp/rustfs.zip -d /tmp/rustfs \
    && install -m 0755 /tmp/rustfs/rustfs /usr/local/bin/rustfs \
    && rm -rf /tmp/rustfs.zip /tmp/rustfs \
    && curl -fsSL "https://dl.min.io/client/mc/release/linux-${MC_ARCH}/mc" -o /usr/local/bin/mc \
    && chmod 0755 /usr/local/bin/mc

ENV RUSTFS_VOLUMES=/data \
    RUSTFS_OBS_LOG_DIRECTORY=/logs
RUN mkdir -p /data /logs

EXPOSE 9000 9001
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/rustfs"]
CMD ["/data"]
