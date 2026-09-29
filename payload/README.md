# Payload

These files are embedded in `TempleOS.exe` and are checked by hash at build
time.

| File | What | Pin |
|---|---|---|
| `bios.bin` | SeaBIOS 1.16.3, the 256 KiB build QEMU uses (`bios-256k.bin`) | `SHA256SUMS` |
| `vgabios.bin` | SeaVGABIOS 1.16.3, the standard-VGA build (`vgabios-stdvga.bin`) | `SHA256SUMS` |
| `TempleOS.ISO` | TempleOS V5.03, the final templeos.org build. Not committed; fetch with `tools/fetch-payload.sh` | `TempleOS.ISO.pin` |

Check the BIOS blobs with `cd payload && sha256sum -c SHA256SUMS`.

## Provenance

- **SeaBIOS / SeaVGABIOS** come from Ubuntu 24.04's `seabios` package,
  version `1.16.3-2` (`/usr/share/seabios/`). The build banner inside them
  is `1.16.3-debian-1.16.3-2`. Both are LGPLv3. The corresponding source is
  upstream SeaBIOS 1.16.3
  (<https://www.seabios.org/downloads/seabios-1.16.3.tar.gz>) plus the
  Debian/Ubuntu source package `seabios 1.16.3-2` (`apt-get source seabios`
  on Ubuntu 24.04). We use the same blobs QEMU uses so that our VMM
  and the QEMU reference traces run identical firmware.
- **TempleOS.ISO** is public domain. The size (17350656 bytes) and MD5
  (`2facf5d7cfa08de4c47aede4a64cfb44`) are the values the tinkeros project
  published for `templeos.org/Downloads/TempleOS.ISO`. The kernel source
  for this build is the `final` tag of
  <https://github.com/cia-foundation/TempleOS>.
