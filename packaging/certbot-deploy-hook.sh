#!/bin/sh
# certbot deploy hook: copies the renewed certificate of the server's host
# name to /etc/ondura-server/tls, readable by the ondura group. Ondura Server
# notices the new files on its own (no restart needed).
#
# Install:
#   ln -s /usr/share/ondura-server/certbot-deploy-hook.sh /etc/letsencrypt/renewal-hooks/deploy/ondura-server
# and run it once after the first `certbot certonly`:
#   RENEWED_LINEAGE=/etc/letsencrypt/live/mail.example.com /usr/share/ondura-server/certbot-deploy-hook.sh
set -eu
config=/etc/ondura-server/config.toml
host=$(sed -n 's/^hostname *= *"\(.*\)"/\1/p' "$config" | head -n1)
lineage=${RENEWED_LINEAGE:-/etc/letsencrypt/live/$host}
case "$lineage" in
    */live/"$host") ;;
    *) exit 0 ;;
esac
install -d -o root -g ondura -m 0750 /etc/ondura-server/tls
install -o root -g ondura -m 0640 "$lineage/fullchain.pem" /etc/ondura-server/tls/fullchain.pem.new
install -o root -g ondura -m 0640 "$lineage/privkey.pem" /etc/ondura-server/tls/privkey.pem.new
mv /etc/ondura-server/tls/privkey.pem.new /etc/ondura-server/tls/privkey.pem
mv /etc/ondura-server/tls/fullchain.pem.new /etc/ondura-server/tls/fullchain.pem
