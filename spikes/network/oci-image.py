#!/usr/bin/env python3
"""Packs a static binary into a one-layer OCI image archive, with no Docker daemon or registry.

    python3 oci-image.py BINARY ARCH IMAGE_NAME OUT.tar
    python3 oci-image.py dist/arm64/cof-network-spike arm64 localhost/chance-of-files-spike:0.1.0 spike-arm64.tar

The image holds only /usr/local/bin/cof-network-spike (a static musl build needs nothing else:
no libc, certificates or DNS). `k3s ctr images import OUT.tar` loads it on the node. This is the
fallback for hosts without a usable Docker; the Dockerfile builds the debian-slim image instead.
"""

import hashlib
import io
import json
import sys
import tarfile

ENTRY = "usr/local/bin/cof-network-spike"


def add(tar: tarfile.TarFile, name: str, data: bytes | None, mode: int) -> None:
    info = tarfile.TarInfo(name)
    info.mtime = 0
    info.uid = info.gid = 0
    info.mode = mode
    if data is None:
        info.type = tarfile.DIRTYPE
        tar.addfile(info)
    else:
        info.size = len(data)
        tar.addfile(info, io.BytesIO(data))


def tar_bytes(files: list[tuple[str, bytes | None, int]]) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name, data, mode in files:
            add(tar, name, data, mode)
    return buf.getvalue()


def blob(blobs: dict[str, bytes], data: bytes) -> str:
    digest = "sha256:" + hashlib.sha256(data).hexdigest()
    blobs[digest] = data
    return digest


def main() -> None:
    if len(sys.argv) != 5:
        sys.exit(__doc__)
    binary, arch, name, out = sys.argv[1:]
    with open(binary, "rb") as f:
        exe = f.read()
    if exe[:4] != b"\x7fELF":
        sys.exit(f"{binary} is not an ELF binary")

    layer = tar_bytes(
        [
            ("usr", None, 0o755),
            ("usr/local", None, 0o755),
            ("usr/local/bin", None, 0o755),
            (ENTRY, exe, 0o755),
            ("tmp", None, 0o1777),
        ]
    )
    blobs: dict[str, bytes] = {}
    layer_digest = blob(blobs, layer)
    config = json.dumps(
        {
            "architecture": arch,
            "os": "linux",
            "config": {
                "User": "65532:65532",
                "Env": ["SPIKE_ADDR=127.0.0.1:8080"],
                "Entrypoint": ["/" + ENTRY],
            },
            "rootfs": {"type": "layers", "diff_ids": [layer_digest]},
        },
        separators=(",", ":"),
    ).encode()
    config_digest = blob(blobs, config)
    manifest = json.dumps(
        {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": len(config),
            },
            "layers": [
                {
                    "mediaType": "application/vnd.oci.image.layer.v1.tar",
                    "digest": layer_digest,
                    "size": len(layer),
                }
            ],
        },
        separators=(",", ":"),
    ).encode()
    manifest_digest = blob(blobs, manifest)
    tag = name.rsplit(":", 1)[1] if ":" in name.rsplit("/", 1)[-1] else "latest"
    index = json.dumps(
        {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": manifest_digest,
                    "size": len(manifest),
                    "platform": {"architecture": arch, "os": "linux"},
                    "annotations": {
                        # containerd (k3s ctr images import) names the image from this one.
                        "io.containerd.image.name": name,
                        "org.opencontainers.image.ref.name": tag,
                    },
                }
            ],
        },
        separators=(",", ":"),
    ).encode()

    files: list[tuple[str, bytes | None, int]] = [
        ("oci-layout", b'{"imageLayoutVersion":"1.0.0"}', 0o644),
        ("index.json", index, 0o644),
        ("blobs", None, 0o755),
        ("blobs/sha256", None, 0o755),
    ]
    files += [(f"blobs/sha256/{d.split(':', 1)[1]}", data, 0o644) for d, data in blobs.items()]
    with open(out, "wb") as f:
        f.write(tar_bytes(files))
    print(f"{out}: {name} ({arch}), manifest {manifest_digest}")


if __name__ == "__main__":
    main()
