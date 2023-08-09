ARG BASE_IMAGE
FROM $BASE_IMAGE

RUN apt update
RUN apt install -y xvfb libxcb1-dev
COPY script.sh .
ENTRYPOINT ["./script.sh"]
