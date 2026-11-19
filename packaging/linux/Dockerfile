# Build image for the portable Linux release (glibc 2.35): the same setup as the Linux jobs of
# .github/workflows/build.yml. scripts/build-linux-release.sh builds and uses it.
FROM ubuntu:22.04@sha256:9abcaffc92a5279fff3a6a41635c0800ea33e528425fb389658806476e7defde
COPY setup-build-env.sh /usr/local/sbin/setup-build-env.sh
RUN /usr/local/sbin/setup-build-env.sh
