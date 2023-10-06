ARG BASE_IMAGE
FROM $BASE_IMAGE

RUN apt update
RUN DEBIAN_FRONTEND=noninteractive TZ=Etc/UTC apt-get -y --no-install-recommends install tzdata libgtk-3-dev
