ARG IMAGE_UPSTREAM=docker.io/library/ubuntu:22.04
ARG OS_VARIANT=debian
ARG ARCHITECTURE=$BUILDPLATFORM

FROM --platform=${ARCHITECTURE} ${IMAGE_UPSTREAM} as base-image-debian
ENV APT_UPDATE="apt update"
ENV APT_INSTALL="apt install -y --no-install-recommends --no-install-suggests"
ENV APT_CLEAN="rm -rf /var/lib/apt/lists/*"
ENV DEBIAN_FRONTEND=noninteractive
RUN $APT_UPDATE && $APT_INSTALL clang curl build-essential cmake pkg-config libssl-dev ca-certificates git protobuf-compiler libprotobuf-dev xvfb && $APT_CLEAN

FROM --platform=${ARCHITECTURE} ${IMAGE_UPSTREAM} as base-image-fedora
ENV DNF_INSTALL="dnf install --setopt=install_weak_deps=False -y"
ENV DNF_CLEAN="dnf clean all"
ENV DNF_COMMON_PKGS="gcc openssl-devel cmake git protobuf-compiler protobuf-devel xorg-x11-server-Xvfb"
WORKDIR /root

RUN dnf config-manager --set-enabled crb || true

RUN bash -c ' \
    if dnf list available "clang-19.*" >/dev/null 2>&1; then \
        $DNF_INSTALL "clang-19.*" $DNF_COMMON_PKGS; \
    else \
        $DNF_INSTALL "clang-18.*" $DNF_COMMON_PKGS; \
    fi \
    && $DNF_CLEAN'


FROM --platform=${ARCHITECTURE} base-image-${OS_VARIANT} as base-rust-image
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt
ENV PATH="/root/.cargo/bin:$PATH"
RUN cargo install cargo-deny

FROM --platform=${ARCHITECTURE} base-rust-image as rust-image-debian
RUN cargo install cargo-deb

FROM --platform=${ARCHITECTURE} base-rust-image as rust-image-fedora
RUN cargo install cargo-generate-rpm

FROM --platform=${ARCHITECTURE} rust-image-${OS_VARIANT} as rust-image

FROM --platform=${ARCHITECTURE} rust-image as fips-capable-image
ARG ARCHITECTURE
ENV CC="clang"
ENV CXX="clang++"
RUN curl -sSf https://dl.google.com/go/go1.22.2.linux-${ARCHITECTURE}.tar.gz | tar -xz -C /usr/local
ENV PATH="$PATH:/usr/local/go/bin"

FROM --platform=${ARCHITECTURE} fips-capable-image as client-image-debian
ENV TZ=Etc/UTC
RUN $APT_UPDATE && $APT_INSTALL tzdata libgtk-3-dev lsb-release && $APT_CLEAN

FROM --platform=${ARCHITECTURE} fips-capable-image as client-image-fedora
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

FROM --platform=${ARCHITECTURE} fips-capable-image as client-image-windows
COPY --from=pe-util-build-windows /root/pe-util/build/peldd /usr/bin/peldd
RUN $DNF_INSTALL mingw64-gcc mingw64-gcc-c++ mingw64-freetype mingw64-cairo mingw64-harfbuzz mingw64-pango mingw64-poppler mingw64-gtk3 mingw64-winpthreads-static mingw64-glib2-static mingw64-openssl boost zip jq nasm bindgen && $DNF_CLEAN
ENV PKG_CONFIG_ALLOW_CROSS=1
ENV PKG_CONFIG_PATH=/usr/x86_64-w64-mingw32/sys-root/mingw/lib/pkgconfig/
ENV GTK_INSTALL_PATH=/usr/x86_64-w64-mingw32/sys-root/mingw/
ENV CARGO_BUILD_TARGET=x86_64-pc-windows-gnu
RUN rustup target add x86_64-pc-windows-gnu

FROM --platform=${ARCHITECTURE} client-image-${OS_VARIANT} as client-image

FROM --platform=${ARCHITECTURE} fips-capable-image as server-image-debian
RUN $APT_UPDATE && $APT_INSTALL libxcb1-dev ubuntu-session mesa-utils lsb-release && $APT_CLEAN

FROM --platform=${ARCHITECTURE} fips-capable-image as server-image-fedora
RUN $DNF_INSTALL -y glx-utils libxcb-devel mesa-dri-drivers && $DNF_CLEAN

FROM --platform=${ARCHITECTURE} server-image-${OS_VARIANT} as server-image
