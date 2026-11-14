FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libstdc++6 procps tini \
    && ln -s /usr/bin/tini /sbin/tini \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/fube
COPY kuberic-operator kuberic-node fube-node-real /opt/kuberic/
RUN ln -s /opt/kuberic/kuberic-node /opt/fube/fube-node \
    && ln -s /opt/kuberic/fube-node-real /opt/fube/fube-node-real
ENTRYPOINT ["/sbin/tini", "-g", "--", "/opt/kuberic/kuberic-operator"]
CMD ["run"]
