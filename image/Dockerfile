# The root filesystem every job's virtual machine starts from: GitHub's Actions runner image plus what the CodeFusion
# workflows use, so a job does not install it each time. Packed and published by .github/workflows/image.yml.
#
# Not pinned on purpose: scale sets switch the Actions runner's self-update off, and GitHub refuses a version 30 days
# after the next release, so the weekly rebuild must pick up each new release by itself.
FROM ghcr.io/actions/actions-runner:latest

ARG NODE_VERSION=24.21.0
ARG NODE_SHA256=6ad1325edbdb5649c379b75a237147a666c95d4f9ae8d340fef2d1575d289ad2
# Only for the system libraries browsers need; each repository still installs its own Playwright and browsers.
ARG PLAYWRIGHT_VERSION=1.63.0

USER root

# Tools workflows call directly, and a compiler for npm packages with native parts.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates gnupg lsof procps psmisc xz-utils zip zstd \
 && mkdir -p -m 755 /etc/apt/keyrings \
 && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=arm64 signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*

# Node 24 in the tool cache, where actions/setup-node finds it for `node-version: 24` without a download, and on
# the PATH for steps that run without setup-node. Each job's tool cache is its own: shared, a job could leave a
# program there for the next one to run.
ENV RUNNER_TOOL_CACHE=/opt/hostedtoolcache AGENT_TOOLSDIRECTORY=/opt/hostedtoolcache
RUN node=/opt/hostedtoolcache/node/${NODE_VERSION}/arm64 \
 && curl -fsSLo /tmp/node.tar.xz "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-arm64.tar.xz" \
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

# Jobs install tools and browsers here, each into its own machine, as the user `runner` the Actions runner works as.
RUN mkdir -p /home/runner/.npm /home/runner/.cache \
 && chown -R runner:runner /opt/hostedtoolcache /home/runner/.npm /home/runner/.cache

USER runner
