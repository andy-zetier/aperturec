ARG IMAGE_UPSTREAM=docker.io/library/ubuntu:24.04
ARG OS_VARIANT=debian
ARG ARCHITECTURE=$TARGETARCH

# Package lists for different image types and OS variants
# Client and server packages
ARG CLIENT_PKGS_DEBIAN="\
    libgtk-3-dev \
    lsb-release \
    tzdata"
ARG CLIENT_PKGS_FEDORA="\
    gtk3-devel \
    tzdata"
ARG SERVER_PKGS_DEBIAN="\
    libxcb1-dev \
    lsb-release \
    mesa-utils \
    ubuntu-session"
ARG SERVER_PKGS_FEDORA="\
    glx-utils \
    libxcb-devel"

# Base image packages
ARG BASE_PKGS_DEBIAN="\
    build-essential \
    ca-certificates \
    cmake \
    curl \
    git \
    gnupg \
    libprotobuf-dev \
    libssl-dev \
    lsb-release \
    pkg-config \
    protobuf-compiler \
    software-properties-common \
    wget \
    xvfb"
ARG BASE_PKGS_FEDORA="\
    cmake \
    dnf-plugins-core \
    gcc \
    git \
    openssl-devel \
    protobuf-compiler \
    protobuf-devel \
    xorg-x11-server-Xvfb"

# PE util build packages (for Windows cross-compilation)
ARG PE_UTIL_PKGS="\
    boost-devel \
    gcc-c++ \
    glibc-static \
    libstdc++-static"

# Windows client packages
ARG CLIENT_PKGS_WINDOWS="\
    azure-cli \
    boost \
    java-latest-openjdk \
    jq \
    mingw64-cairo \
    mingw64-freetype \
    mingw64-gcc \
    mingw64-gcc-c++ \
    mingw64-glib2-static \
    mingw64-gtk3 \
    mingw64-harfbuzz \
    mingw64-openssl \
    mingw64-pango \
    mingw64-poppler \
    mingw64-winpthreads-static \
    msitools \
    nasm \
    pandoc \
    util-linux \
    zip"

FROM ${IMAGE_UPSTREAM} AS base-image-debian
ARG BASE_PKGS_DEBIAN
ENV APT_UPDATE="apt update"
ENV APT_INSTALL="apt install -y --no-install-recommends --no-install-suggests"
ENV APT_CLEAN="rm -rf /var/lib/apt/lists/*"
ENV DEBIAN_FRONTEND=noninteractive
ENV OPENSSL_FORCE_FIPS_MODE=0
RUN $APT_UPDATE && $APT_INSTALL ${BASE_PKGS_DEBIAN} && $APT_CLEAN
RUN wget https://apt.llvm.org/llvm.sh && \
    chmod +x llvm.sh && \
    ./llvm.sh 19 && \
    rm llvm.sh && \
    $APT_CLEAN

FROM ${IMAGE_UPSTREAM} AS base-image-fedora
ARG BASE_PKGS_FEDORA
ENV DNF_INSTALL="dnf install --setopt=install_weak_deps=False -y"
ENV DNF_CLEAN="dnf clean all"
WORKDIR /root
RUN dnf config-manager --set-enabled crb || true
RUN $DNF_INSTALL ${BASE_PKGS_FEDORA}
RUN . /etc/os-release && \
    ARCH=$(rpm --eval '%{_arch}') && \
    if [ "$ID" = "centos" ]; then \
        COPR_TARGET="centos-stream-${VERSION_ID}-${ARCH}"; \
    else \
        COPR_TARGET="${ID}-${VERSION_ID}-${ARCH}"; \
    fi && \
    dnf copr enable -y @fedora-llvm-team/llvm-snapshots "$COPR_TARGET" && \
    if dnf list --available clang-19 >/dev/null 2>&1; then \
        CLANG_PKG="clang-19"; \
    elif dnf list --available clang19 >/dev/null 2>&1; then \
        CLANG_PKG="clang19"; \
    else \
        echo "Failed to locate clang 19 packages for $COPR_TARGET" >&2; \
        exit 1; \
    fi && \
    $DNF_INSTALL $CLANG_PKG && $DNF_CLEAN

FROM base-image-${OS_VARIANT} AS base-rust-image
ENV CC="clang-19"
ENV CXX="clang++-19"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt
ENV PATH="/root/.cargo/bin:$PATH"
RUN rustup default stable
RUN rustup component add clippy
RUN cargo install cargo-deny

FROM base-rust-image AS rust-image-debian
RUN cargo install cargo-deb

FROM base-rust-image AS rust-image-fedora
RUN cargo install cargo-generate-rpm

FROM rust-image-${OS_VARIANT} AS rust-image

FROM rust-image AS fips-capable-image
ARG ARCHITECTURE
RUN curl -sSf https://dl.google.com/go/go1.22.2.linux-${ARCHITECTURE}.tar.gz | tar -xz -C /usr/local
ENV PATH="$PATH:/usr/local/go/bin"

FROM fips-capable-image AS client-image-debian
ARG CLIENT_PKGS_DEBIAN
ENV TZ=Etc/UTC
RUN $APT_UPDATE && $APT_INSTALL ${CLIENT_PKGS_DEBIAN} && $APT_CLEAN

FROM fips-capable-image AS client-image-fedora
ARG CLIENT_PKGS_FEDORA
RUN $DNF_INSTALL ${CLIENT_PKGS_FEDORA} && $DNF_CLEAN

FROM base-image-fedora AS pe-util-build-windows
ARG PE_UTIL_PKGS
RUN $DNF_INSTALL ${PE_UTIL_PKGS} && $DNF_CLEAN
RUN git clone https://github.com/gsauthof/pe-util
WORKDIR pe-util
RUN git submodule update --init
RUN mkdir build
WORKDIR build
RUN cmake .. -DCMAKE_BUILD_TYPE=Release -DCMAKE_EXE_LINKER_FLAGS="-static" -DCMAKE_FIND_LIBRARY_SUFFIXES=".a"
RUN make

FROM fips-capable-image AS client-image-windows
ARG CLIENT_PKGS_WINDOWS
COPY --from=pe-util-build-windows /root/pe-util/build/peldd /usr/bin/peldd
RUN $DNF_INSTALL ${CLIENT_PKGS_WINDOWS} && $DNF_CLEAN

ENV PKG_CONFIG_ALLOW_CROSS=1
ENV PKG_CONFIG_PATH=/usr/x86_64-w64-mingw32/sys-root/mingw/lib/pkgconfig/
ENV GTK_INSTALL_PATH=/usr/x86_64-w64-mingw32/sys-root/mingw/
ENV CARGO_BUILD_TARGET=x86_64-pc-windows-gnu
RUN rustup target add x86_64-pc-windows-gnu

FROM client-image-${OS_VARIANT} AS client-image

FROM fips-capable-image AS server-image-debian
ARG SERVER_PKGS_DEBIAN
RUN $APT_UPDATE && $APT_INSTALL ${SERVER_PKGS_DEBIAN} && $APT_CLEAN

FROM fips-capable-image AS server-image-fedora
ARG SERVER_PKGS_FEDORA
RUN $DNF_INSTALL ${SERVER_PKGS_FEDORA} && $DNF_CLEAN

FROM server-image-${OS_VARIANT} AS server-image

FROM fips-capable-image AS combined-image-debian
ARG CLIENT_PKGS_DEBIAN
ARG SERVER_PKGS_DEBIAN
ENV TZ=Etc/UTC
RUN $APT_UPDATE && $APT_INSTALL ${CLIENT_PKGS_DEBIAN} ${SERVER_PKGS_DEBIAN} && $APT_CLEAN

FROM fips-capable-image AS combined-image-fedora
ARG CLIENT_PKGS_FEDORA
ARG SERVER_PKGS_FEDORA
RUN $DNF_INSTALL ${CLIENT_PKGS_FEDORA} ${SERVER_PKGS_FEDORA} && $DNF_CLEAN

FROM combined-image-${OS_VARIANT} AS combined-image
