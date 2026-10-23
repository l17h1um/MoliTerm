# Dev test servers

```
docker compose -f dev/docker-compose.yml up -d --build
cargo test --features it          # integration tests against these servers
```

| Service | Address | Login |
|---|---|---|
| sshd (SFTP) | localhost:2222 | test / test, or key `dev/keys/test_ed25519` |
| sshd without SFTP | localhost:2223 | same |
| bastion | localhost:2224 | same; jump to `target:22` |
| telnetd | localhost:2323 | test / test |
| xrdp | localhost:3390 | see the image docs |

`dev/keys/test_ed25519_enc` is the same kind of key protected with passphrase `testpass`.
To test `.ppk`, convert a key with PuTTYgen (`puttygen dev/keys/test_ed25519 -o test.ppk`), then add the `.pub` to `dev/sshd/authorized_keys` and rebuild.

Serial: create a pty pair with `socat -d -d pty,raw,echo=0 pty,raw,echo=0` and open one end in MoliTerm (Phase 5).

These keys are for local testing only.
