# The conformance tools m21 runs, pinned: a known-failure list means
# something only against the suite it was taken from.
FROM debian:trixie
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    build-essential autoconf automake libtool pkg-config git perl ca-certificates python3 \
    xfslibs-dev uuid-dev libattr1-dev libacl1-dev libaio-dev libgdbm-dev libssl-dev liburing-dev libcap-dev >/dev/null
RUN git clone -q https://github.com/pjd/pjdfstest /opt/pjdfstest \
    && cd /opt/pjdfstest && git checkout -q 85a8aea9e685999ef0540392fd80535f873d7ff7 \
    && autoreconf -ifs >/dev/null 2>&1 && ./configure -q && make -s pjdfstest
RUN git clone -q https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git /opt/xfstests \
    && cd /opt/xfstests && git checkout -q 22348afe338c0f6d540c0d7ef0db749eaa51b218 \
    && make -s >/dev/null 2>&1 && test -x ltp/fsx
