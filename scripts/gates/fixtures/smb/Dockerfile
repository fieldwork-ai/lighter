FROM alpine:3.21
RUN apk add --no-cache samba && mkdir -p /srv/d /srv/t && chmod 777 /srv/d /srv/t \
 && adduser -D -H lt && printf 'lt\nlt\n' | smbpasswd -a -s lt
COPY smb.conf /etc/samba/smb.conf
CMD ["smbd", "--foreground", "--no-process-group", "--debug-stdout"]
