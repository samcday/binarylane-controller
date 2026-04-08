FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    curl -fsSL https://github.com/tilt-dev/tilt/releases/download/v0.33.22/tilt.0.33.22.linux.x86_64.tar.gz -o /tmp/tilt.tar.gz; \
    echo "f927dbaf7cabebaa7bc71c111d5f045bf8e46a3f0b97eb8735e4133d30cdc670  /tmp/tilt.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/tilt.tar.gz -C /usr/local/bin tilt; \
    rm -f /tmp/tilt.tar.gz

RUN set -eux; \
    curl -fsSL https://github.com/google/go-containerregistry/releases/download/v0.21.3/go-containerregistry_Linux_x86_64.tar.gz -o /tmp/go-containerregistry.tar.gz; \
    echo "46dbf12d943efa5673ab654186c5d7c1503580544de0df9325537083436fe5d0  /tmp/go-containerregistry.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/go-containerregistry.tar.gz -C /tmp crane; \
    install -m 0755 /tmp/crane /usr/local/bin/crane; \
    rm -f /tmp/go-containerregistry.tar.gz /tmp/crane
