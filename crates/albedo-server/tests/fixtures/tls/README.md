# TLS test fixtures

A self-signed `CN=localhost` certificate (SAN `DNS:localhost`, `IP:127.0.0.1`),
valid 10 years from 2026-09-03, and its unencrypted PKCS#8 key.

**It is a test fixture and nothing else.** It is self-signed, its private key is
committed in the clear beside it, and no deployment should ever present it. It
exists so `tls.rs` can prove it loads a real PEM pair and so the HTTPS end-to-end
test has something to serve, without requiring `openssl` on the machine running
the suite.

Regenerate with:

    openssl req -x509 -newkey rsa:2048 -keyout localhost-key.pem \
      -out localhost-cert.pem -days 3650 -nodes -subj "/CN=localhost" \
      -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
