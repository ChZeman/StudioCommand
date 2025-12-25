# Packaging notes

`packaging/install.sh` installs StudioCommand to `/opt/studiocommand` and configures Nginx to serve HTTPS on port **8443**.

- Always provide `--domain`.
- Provide `--email` to obtain a Let's Encrypt certificate automatically.
- If `--email` is omitted or certbot fails, the installer generates a **self-signed** certificate.

Nginx template: `packaging/nginx-studiocommand.conf`
