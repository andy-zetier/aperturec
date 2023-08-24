ARG BASE_IMAGE=ubuntu:20.04
FROM $BASE_IMAGE

RUN apt update
RUN apt install -y curl build-essential
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:$PATH"
