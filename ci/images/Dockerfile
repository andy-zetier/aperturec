ARG IMAGE_BASE=ubuntu
ARG OS_VARIANT=debian
ARG ARCHITECTURE=$BUILDPLATFORM
ARG UPSTREAM_REGISTRY=docker.io

FROM --platform=${ARCHITECTURE} ${UPSTREAM_REGISTRY}/${IMAGE_BASE} as base-image-debian
ENV APT_UPDATE="apt update"
ENV APT_INSTALL="apt install -y --no-install-recommends --no-install-suggests"
ENV APT_CLEAN="rm -rf /var/lib/apt/lists/*"
ENV DEBIAN_FRONTEND=noninteractive
RUN $APT_UPDATE && $APT_INSTALL curl build-essential cmake pkg-config libssl-dev ca-certificates git protobuf-compiler libprotobuf-dev && $APT_CLEAN

FROM --platform=${ARCHITECTURE} ${UPSTREAM_REGISTRY}/${IMAGE_BASE} as base-image-fedora
ENV DNF_INSTALL="dnf install --setopt=install_weak_deps=False -y"
ENV DNF_CLEAN="dnf clean all"
WORKDIR /root
RUN $DNF_INSTALL -y gcc openssl-devel cmake git protobuf-compiler protobuf-devel && $DNF_CLEAN

FROM --platform=${ARCHITECTURE} base-image-${OS_VARIANT} as base-rust-image
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt
ENV PATH="/root/.cargo/bin:$PATH"

FROM --platform=${ARCHITECTURE} base-rust-image as rust-image-debian
RUN cargo install cargo-deb

FROM --platform=${ARCHITECTURE} base-rust-image as rust-image-fedora

FROM --platform=${ARCHITECTURE} rust-image-${OS_VARIANT} as rust-image

FROM --platform=${ARCHITECTURE} rust-image as client-image-debian
ENV TZ=Etc/UTC
RUN $APT_UPDATE && $APT_INSTALL tzdata libgtk-3-dev lsb-release && $APT_CLEAN

FROM --platform=${ARCHITECTURE} rust-image as client-image-fedora
RUN $DNF_INSTALL tzdata gtk3-devel && $DNF_CLEAN

FROM --platform=${ARCHITECTURE} base-image-fedora as pe-util-build-windows
RUN $DNF_INSTALL -y gcc-c++ boost-devel libstdc++-static glibc-static && $DNF_CLEAN
RUN git clone https://github.com/gsauthof/pe-util
WORKDIR pe-util
RUN git submodule update --init
RUN mkdir build
WORKDIR build
RUN cmake .. -DCMAKE_BUILD_TYPE=Release -DCMAKE_EXE_LINKER_FLAGS="-static" -DCMAKE_FIND_LIBRARY_SUFFIXES=".a"
RUN make

FROM --platform=${ARCHITECTURE} rust-image as client-image-windows
COPY --from=pe-util-build-windows /root/pe-util/build/peldd /usr/bin/peldd
RUN $DNF_INSTALL mingw64-gcc mingw64-freetype mingw64-cairo mingw64-harfbuzz mingw64-pango mingw64-poppler mingw64-gtk3 mingw64-winpthreads-static mingw64-glib2-static mingw64-openssl boost zip jq && $DNF_CLEAN
ENV PKG_CONFIG_ALLOW_CROSS=1
ENV PKG_CONFIG_PATH=/usr/x86_64-w64-mingw32/sys-root/mingw/lib/pkgconfig/
ENV GTK_INSTALL_PATH=/usr/x86_64-w64-mingw32/sys-root/mingw/
ENV CARGO_BUILD_TARGET=x86_64-pc-windows-gnu
RUN rustup target add x86_64-pc-windows-gnu

FROM --platform=${ARCHITECTURE} client-image-${OS_VARIANT} as client-image

FROM --platform=${ARCHITECTURE} rust-image as server-image-debian
RUN $APT_UPDATE && $APT_INSTALL xvfb libxcb1-dev ubuntu-session mesa-utils lsb-release && $APT_CLEAN

FROM --platform=${ARCHITECTURE} rust-image as server-image-fedora
RUN $DNF_INSTALL -y glx-utils xorg-x11-server-Xvfb libxcb-devel mesa-dri-drivers && $DNF_CLEAN

FROM --platform=${ARCHITECTURE} server-image-${OS_VARIANT} as server-image
