ARG BASE_IMAGE
FROM $BASE_IMAGE

ENV DEBIAN_FRONTEND=noninteractive
RUN apt update
RUN apt install -y \
    xvfb \
    libxcb1-dev \
    ubuntu-session \
    mesa-utils
