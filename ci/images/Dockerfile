ARG IMAGE_BASE=ubuntu
ARG OS_VARIANT=debian
ARG ARCHITECTURE=$BUILDPLATFORM
ARG UPSTREAM_REGISTRY=docker.io

FROM --platform=${ARCHITECTURE} ${UPSTREAM_REGISTRY}/${IMAGE_BASE} as base-image-debian
ENV APT_UPDATE="apt update"
ENV APT_INSTALL="apt install -y --no-install-recommends --no-install-suggests"
ENV APT_CLEAN="rm -rf /var/lib/apt/lists/*"
ENV DEBIAN_FRONTEND=noninteractive
RUN $APT_UPDATE && $APT_INSTALL curl build-essential cmake pkg-config libssl-dev ca-certificates git && $APT_CLEAN

FROM --platform=${ARCHITECTURE} ${UPSTREAM_REGISTRY}/${IMAGE_BASE} as base-image-fedora
ENV DNF_INSTALL="dnf install --setopt=install_weak_deps=False -y"
ENV DNF_CLEAN="dnf clean all"
WORKDIR /root
RUN $DNF_INSTALL -y gcc openssl-devel cmake git && $DNF_CLEAN

FROM --platform=${ARCHITECTURE} base-image-${OS_VARIANT} as base-rust-image
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt
ENV PATH="/root/.cargo/bin:$PATH"

FROM --platform=${ARCHITECTURE} base-rust-image as rust-image-debian
RUN cargo install cargo-deb

FROM --platform=${ARCHITECTURE} base-rust-image as rust-image-fedora

FROM --platform=${ARCHITECTURE} rust-image-${OS_VARIANT} as rust-image

FROM --platform=${ARCHITECTURE} rust-image as asn1c-base-image-debian
RUN $APT_UPDATE && $APT_INSTALL git autoconf automake libtool bison flex make clang && $APT_CLEAN

FROM --platform=${ARCHITECTURE} rust-image as asn1c-base-image-fedora
RUN $DNF_INSTALL -y git autoconf libtool bison flex make clang && $DNF_CLEAN

FROM --platform=${ARCHITECTURE} asn1c-base-image-${OS_VARIANT} as asn1c-image
WORKDIR /root
RUN git clone https://github.com/vlm/asn1c
WORKDIR /root/asn1c
RUN autoreconf -iv
RUN ./configure
RUN make -j
RUN make -j install
WORKDIR /root

FROM --platform=${ARCHITECTURE} rust-image as client-image-debian
ENV TZ=Etc/UTC
RUN $APT_UPDATE && $APT_INSTALL tzdata libgtk-3-dev lsb-release && $APT_CLEAN

FROM --platform=${ARCHITECTURE} rust-image as client-image-fedora
RUN $DNF_INSTALL tzdata gtk3-devel && $DNF_CLEAN

FROM --platform=${ARCHITECTURE} client-image-${OS_VARIANT} as client-image

FROM --platform=${ARCHITECTURE} rust-image as server-image-debian
RUN $APT_UPDATE && $APT_INSTALL xvfb libxcb1-dev ubuntu-session mesa-utils lsb-release && $APT_CLEAN

FROM --platform=${ARCHITECTURE} rust-image as server-image-fedora
RUN $DNF_INSTALL -y glx-utils xorg-x11-server-Xvfb libxcb-devel mesa-dri-drivers && $DNF_CLEAN

FROM --platform=${ARCHITECTURE} server-image-${OS_VARIANT} as server-image
