# The root filesystem every job's virtual machine starts from: GitHub's Actions runner image plus what the CodeFusion
# workflows use, so a job does not install it each time. Packed and published by .github/workflows/image.yml.
#
# Not pinned on purpose: scale sets switch the Actions runner's self-update off, and GitHub refuses a version 30 days
# after the next release, so the weekly rebuild must pick up each new release by itself.
#
# One file for both architectures a host runs (arm64 and amd64): Docker's builder sets TARGETARCH to the platform it
# builds, and each download below is pinned for each.
FROM ghcr.io/actions/actions-runner:latest

ARG TARGETARCH
ARG NODE_VERSION=24.21.0
ARG NODE_SHA256_arm64=6ad1325edbdb5649c379b75a237147a666c95d4f9ae8d340fef2d1575d289ad2
ARG NODE_SHA256_amd64=fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6
# Only for the system libraries browsers need; each repository still installs its own Playwright and browsers.
ARG PLAYWRIGHT_VERSION=1.63.0
# Docker Compose for `cf-docker` jobs; the Actions runner image has the engine, the CLI and buildx already.
ARG COMPOSE_VERSION=5.6.0
ARG COMPOSE_SHA256_arm64=733ec76717ceb59052a9609b9dadfb523b2df8eab57a54212872d10a58078ea2
ARG COMPOSE_SHA256_amd64=40343e21ca777173e69cff5dbafeb37c6f81f3b0d57d9e597f036e95eb63e76a

USER root

# The names each project gives the architecture, and a build for any other ends here.
RUN case "${TARGETARCH}" in \
      arm64) printf 'NODE_ARCH=arm64\nNODE_SHA256=%s\nCOMPOSE_ARCH=aarch64\nCOMPOSE_SHA256=%s\n' "${NODE_SHA256_arm64}" "${COMPOSE_SHA256_arm64}" ;; \
      amd64) printf 'NODE_ARCH=x64\nNODE_SHA256=%s\nCOMPOSE_ARCH=x86_64\nCOMPOSE_SHA256=%s\n' "${NODE_SHA256_amd64}" "${COMPOSE_SHA256_amd64}" ;; \
      *) echo "no image is pinned for '${TARGETARCH}'" >&2; exit 1 ;; \
    esac > /tmp/arch.env

# Tools workflows call directly, and a compiler for npm packages with native parts.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates gnupg lsof procps psmisc xz-utils zip zstd \
 && mkdir -p -m 755 /etc/apt/keyrings \
 && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=${TARGETARCH} signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*

# Node 24 in the tool cache, where actions/setup-node finds it for `node-version: 24` without a download, and on
# the PATH for steps that run without setup-node. Each job's tool cache is its own: shared, a job could leave a
# program there for the next one to run.
ENV RUNNER_TOOL_CACHE=/opt/hostedtoolcache AGENT_TOOLSDIRECTORY=/opt/hostedtoolcache
RUN . /tmp/arch.env \
 && node=/opt/hostedtoolcache/node/${NODE_VERSION}/${NODE_ARCH} \
 && curl -fsSLo /tmp/node.tar.xz "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
 && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
 && mkdir -p "$node" \
 && tar -xJf /tmp/node.tar.xz -C "$node" --strip-components=1 --exclude CHANGELOG.md --exclude LICENSE --exclude README.md \
 && touch "$node.complete" \
 && ln -s "$node"/bin/node "$node"/bin/npm "$node"/bin/npx "$node"/bin/corepack /usr/local/bin/ \
 && rm /tmp/node.tar.xz \
 && node --version

# The libraries Chromium, Firefox and WebKit load. `playwright install --with-deps` then finds them installed and
# only downloads the browsers.
RUN npx -y "playwright@${PLAYWRIGHT_VERSION}" install-deps chromium firefox webkit \
 && rm -rf /var/lib/apt/lists/* /root/.npm

# What a `cf-docker` job's daemon needs beside the engine: pasta, which joins the daemon's network to the machine's,
# iptables for the daemon's own rules in that network, and the Compose plugin. The daemon itself runs only in such a
# job, in a sandbox the guest makes (crates/supervisor/src/docker.rs).
RUN . /tmp/arch.env \
 && apt-get update \
 && apt-get install -y --no-install-recommends iptables passt \
 && rm -rf /var/lib/apt/lists/* \
 && curl -fsSLo /usr/local/lib/docker/cli-plugins/docker-compose \
      "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-${COMPOSE_ARCH}" \
 && echo "${COMPOSE_SHA256}  /usr/local/lib/docker/cli-plugins/docker-compose" | sha256sum -c - \
 && chmod 755 /usr/local/lib/docker/cli-plugins/docker-compose \
 && dockerd --version && pasta --version | head -1 && iptables --version && docker compose version

# Jobs install tools and browsers here, each into its own machine, as the user `runner` the Actions runner works as.
RUN rm /tmp/arch.env \
 && mkdir -p /home/runner/.npm /home/runner/.cache \
 && chown -R runner:runner /opt/hostedtoolcache /home/runner/.npm /home/runner/.cache

USER runner
