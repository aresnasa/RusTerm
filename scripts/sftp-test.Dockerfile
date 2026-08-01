FROM ubuntu:22.04

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd \
    && useradd --create-home --shell /bin/bash rusterm-test \
    && echo 'rusterm-test:rusterm-test-only' | chpasswd \
    && mkdir -p /home/rusterm-test/work \
    && chown -R rusterm-test:rusterm-test /home/rusterm-test/work

RUN printf '%s\n' \
    'PermitRootLogin no' \
    'PasswordAuthentication yes' \
    'KbdInteractiveAuthentication no' \
    'UsePAM no' \
    'AllowUsers rusterm-test' \
    'AllowTcpForwarding no' \
    'X11Forwarding no' \
    'Subsystem sftp internal-sftp' \
    > /etc/ssh/sshd_config

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
