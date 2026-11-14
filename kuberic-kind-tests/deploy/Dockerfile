FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libstdc++6 procps tini \
    && ln -s /usr/bin/tini /sbin/tini \
    && rm -rf /var/lib/apt/lists/*
COPY kuberic-kind fube-node fube-node-real /opt/fube/
COPY package /opt/fube/package
