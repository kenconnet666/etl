FROM postgres:17-bookworm

USER root

# Minimal tools for debugging
RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        curl ca-certificates procps less \
    && rm -rf /var/lib/apt/lists/*
