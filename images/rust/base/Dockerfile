ARG BASE_IMAGE=ubuntu:20.04
FROM $BASE_IMAGE

ENV DEBIAN_FRONTEND=noninteractive
RUN apt update
RUN apt install -y curl build-essential cmake pkg-config libssl-dev
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:$PATH"
