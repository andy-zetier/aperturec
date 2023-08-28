ARG BASE_IMAGE
FROM $BASE_IMAGE

RUN apt update
RUN apt install -y git autoconf libtool bison flex make clang

WORKDIR /root
RUN git clone https://github.com/vlm/asn1c

WORKDIR /root/asn1c
RUN autoreconf -iv
RUN ./configure
RUN make -j
RUN make install

WORKDIR /root
