FROM ubuntu:20.04

RUN apt-get update && apt-get install -y libssl-dev ca-certificates

COPY target/release/r5d3 /r5d3

ENTRYPOINT ["/r5d3"]
