# Noyra-sidecar

Minimal mTLS proxy. Terminates mTLS, forwards plaintext to localhost. Hot-reloads certs on SIGHUP.

- All RAM optimizations (madvise, setrlimit, max_fragment, copy buffers, etc.)
- idle RSS: 564 KB on Linux scratch native arm64
- Image size: 788 KB
- 210 lines of Rust

## Env

- `TLS_PORT` (8443), `BACKEND_PORT` (8080), `HEALTH_PORT` (9090)
- `CERT_FILE` (/certs/tls.crt), `KEY_FILE` (/certs/tls.key), `CA_FILE` (/certs/ca.crt)

## Run

```sh
docker build -t noyra-sidecar .
docker run --rm -p 8443:8443 -p 9090:9090 -v $PWD/certs:/certs:ro noyra-sidecar
docker kill --signal=HUP <container>   # reload certs
```
