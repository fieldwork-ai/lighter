FROM alpine:3.21
RUN apk add --no-cache samba \
 && adduser -D -H lt && printf 'lt\nlt\n' | smbpasswd -a -s lt \
 && mkdir -p /srv/d /srv/t /srv/q /srv/n /srv/f && chmod 777 /srv/d /srv/t /srv/n /srv/f \
 && chown lt /srv/q && chmod 2775 /srv/q
COPY smb.conf smb-fid.conf /etc/samba/
CMD ["smbd", "--foreground", "--no-process-group", "--debug-stdout"]
