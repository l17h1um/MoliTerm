# Test SSH server: user `test` / password `test`, keys from dev/keys.
FROM alpine:3.20
ARG SFTP=yes
RUN apk add --no-cache openssh bash xauth \
 && ssh-keygen -A \
 && adduser -D -s /bin/bash test \
 && echo 'test:test' | chpasswd \
 && mkdir -p /home/test/.ssh \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/; s/^#\?AllowTcpForwarding.*/AllowTcpForwarding yes/; s/^#\?KbdInteractiveAuthentication.*/KbdInteractiveAuthentication yes/; s/^#\?X11Forwarding.*/X11Forwarding yes/' /etc/ssh/sshd_config \
 && if [ "$SFTP" = "no" ]; then sed -i '/^Subsystem/d' /etc/ssh/sshd_config; fi
COPY authorized_keys /home/test/.ssh/authorized_keys
RUN chown -R test:test /home/test/.ssh && chmod 700 /home/test/.ssh && chmod 600 /home/test/.ssh/authorized_keys
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
