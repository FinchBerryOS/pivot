# pivot — FinchBerryOS initramfs PID 1
**The first process. The root switcher. The boot-time assembler.**

`pivot` is the PID 1 init binary embedded in the FinchBerryOS initramfs. It is written in **Rust**, intended to be **statically linked** (typically with **musl**), and is responsible for discovering the correct boot partitions, validating the writable system partition, assembling the final root filesystem, handling update mode, and finally handing control over to the real system init: `syscored`.

`pivot` is designed to run on **embedded devices**, **laptops**, and **workstations** without depending on `udev`, `blkid`, or a fully populated userspace.

---

## What `pivot` does

At a high level, `pivot` performs these tasks:

1. Mounts the minimal kernel virtual filesystems required by early userspace
2. Reads `/pivot.config` from the initramfs
3. Locates the Boot Partition and System Partition by **direct GPT PARTUUID lookup**
4. Runs `e2fsck` on the System Partition
5. Mounts the System Partition
6. Optionally enters **RAM update mode**
7. Selects the active A/B system image
8. Mounts the selected **SquashFS** image via a loop device
9. Builds the final root filesystem using a mix of read-only and read-write bind mounts
10. Moves `/dev`, `/proc`, and `/sys` into the new root
11. Frees selected no-longer-needed initramfs content
12. Switches root and `exec()`s `/usr/libexec/syscored`

---

## Design goals

`pivot` is built around a few strict design goals:

- **Run as PID 1 safely**
- **Fail loudly and deterministically**
- **Avoid userspace discovery dependencies**
- **Support A/B system images**
- **Keep the immutable OS read-only**
- **Persist user and application data on the writable system partition**
- **Work in minimal early-boot environments**
- **Support update execution entirely from RAM when required**

This binary is primarily about **boot robustness**, **filesystem assembly**, and **root handoff**. It does **not** currently implement cryptographic image or update signature verification.

---

## Boot flow

### 1. Early VFS setup

`pivot` starts as PID 1 inside the initramfs and immediately mounts:

- `/dev` as **devtmpfs**
- `/proc` as **procfs**
- `/sys` as **sysfs**

`/dev` is mounted first so that `/dev/kmsg` is available as early as possible.

`/tmp` and `/run` are **not** mounted at this stage. They are created later inside the final root filesystem.

---

### 2. Panic handling and fatal errors

A global Rust panic hook is installed before any meaningful setup happens.

Any unrecoverable failure ends in `fatal_error()`, which:

1. Logs the error to `/dev/kmsg` and `stderr`
2. Attempts to trigger a kernel panic by writing `c` to `/proc/sysrq-trigger`
3. Falls back to sending `SIGABRT` to PID 1
4. Enters an infinite sleep loop as a last resort

This ensures that PID 1 never simply exits quietly.

---

### 3. Configuration loading

`pivot` reads `/pivot.config` from the initramfs root and parses it as TOML.

The configuration defines:

- boot mode
- active A/B slot
- GPT PARTUUIDs for the boot and system partitions
- filenames of the slot images stored on the System Partition

Invalid values are rejected immediately.

---

### 4. Partition discovery without `udev`

Instead of using `udev`, `blkid`, or fixed `/dev/sdX` assumptions, `pivot` discovers the System Partition and Boot Partition by:

- enumerating disks in `/sys/class/block`
- skipping non-disk and virtual devices
- opening raw block devices directly
- parsing GPT headers and partition entries manually
- matching the configured PARTUUID against raw GPT partition GUIDs

If the primary GPT header is invalid, `pivot` can fall back to the **backup GPT header** stored at the last LBA of the disk.

This approach allows partition discovery to work in a minimal initramfs with only `devtmpfs` and `sysfs`.

---

### 5. Filesystem check

Before mounting the writable System Partition, `pivot` executes:

```sh
/sbin/e2fsck -p -f <system-partition>