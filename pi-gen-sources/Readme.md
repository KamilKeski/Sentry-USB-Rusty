## Building a Sentry USB Image

> **Prebuilt images aren't offered right now.** Install Sentry USB with the one-liner in the [README](../README.md#install). These image scripts are kept for a future image release and aren't maintained or tested in the meantime.

The Sentry USB image is a custom Raspberry Pi OS image with everything pre-installed. Users just flash it, configure WiFi in Pi Imager, boot, and open the web UI — no SSH required.

### Quick Method (recommended)

From the repo root, run:

```bash
./build-image.sh
```

This will:
1. Build the Sentry USB binaries with `cross` (or download them from releases)
2. Clone pi-gen and inject the binaries
3. Build the image using Docker
4. Output a compressed `.img.gz` in `deploy/`

Takes 15–30 minutes. Requires **Docker**.

You can also pass a pre-built binary:
```bash
./build-image.sh target/aarch64-unknown-linux-gnu/release/sentryusb
```

Add `--32bit` to build the armhf image instead.

### Manual Method

1. Clone pi-gen: `git clone --branch arm64 https://github.com/RPi-Distro/pi-gen` (omit `--branch arm64` for the 32-bit image)
2. From the pi-gen folder, run: `bash /path/to/Sentry-USB-Rusty/pi-gen-sources/prepare.sh` (for 32-bit, then copy `pi-gen-config-32bit` over `config`)
3. Optionally copy local binaries into `stage_sentryusb/00-sentryusb-tweaks/files/` named `sentryusb-<variant>` (e.g. `sentryusb-linux-arm64-a72`); any variant not supplied is downloaded from the latest release
4. Run `./build-docker.sh` (Docker recommended) or `./build.sh`
5. Image will be in `deploy/`. Flash with Raspberry Pi Imager.

### What's in the image

- Raspberry Pi OS Lite, Trixie (64-bit arm64, or armhf with `--32bit`)
- SentryUSB binary variants pre-installed under `/opt/sentryusb/`, with `sentryusb-pick-binary` selecting the best one for the CPU at each service start
- systemd service enabled (web UI on port 80)
- rc.local boot-loop for setup (creates partitions, configures archiving)
- SSH enabled, `dwc2` overlay for USB gadget
- Prerequisite packages from `00-sentryusb-tweaks/00-packages` (e.g. xfsprogs, exfatprogs, dos2unix, autofs, rsync)
- Unnecessary packages removed, swap disabled

### User experience after flashing

1. User flashes image with Pi Imager, configures WiFi + password in settings
2. Boots Pi → WiFi connects → Sentry USB web server starts on port 80
3. User opens `http://sentryusb.local` → sees the dashboard
4. Completes Setup Wizard → Pi reboots several times (10–20 min) → done
5. Plugs into Tesla
