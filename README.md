# pivot

`pivot` is the early boot init binary for FinchBerryOS. It runs as **PID 1** inside the initramfs and prepares the real root filesystem before handing control to `/sbin/syscored`.

It is written in Rust and is designed to work in a minimal early-boot environment without relying on `udev`, `blkid`, or fixed `/dev/sdX` paths.

---

## What it does

At boot, `pivot` performs these main steps:

1. mounts `/dev`, `/proc`, and `/sys`
2. reads `/pivot.config`
3. validates the config
4. finds the Boot Partition and System Partition by GPT PARTUUID
5. runs `e2fsck` on the System Partition
6. mounts the System Partition at `/mnt/system`
7. selects the active A/B system image
8. mounts the selected SquashFS image through a loop device at `/system/base_root`
9. builds the final root filesystem under `/system/rootfs`
10. checks whether update mode should start
11. if no update is requested:

* moves `/dev`, `/proc`, and `/sys` into the new root
* mounts fresh tmpfs instances for `/run` and `/tmp`
* cleans up selected old initramfs files
* switches root and executes `/sbin/syscored`

12. if update mode is requested:

* copies the updater from the active system image into RAM
* unmounts all mounts that still reference the active image
* detaches the loop device
* executes the updater from RAM while the System Partition stays mounted

---

## Main ideas

### A/B boot images

`pivot` supports two system image slots:

* Slot A
* Slot B

The active slot is selected from the config.
The selected image is expected on the System Partition and is mounted read-only as a SquashFS image.

### Read-only system + writable data

The final root filesystem is assembled from:

* a **read-only SquashFS base** for the system itself
* **writable directories** from the System Partition for persistent data
* **tmpfs mounts** for volatile runtime state

This keeps the OS base immutable while user and system data remain persistent.

### No `udev` dependency

Instead of relying on userspace device helpers, `pivot` reads GPT data directly from block devices and matches partitions by PARTUUID.

### Update mode from RAM

If update trigger files are present, `pivot` does **not** start the normal system.

Instead it:

* mounts the active slot image
* copies the updater **from the active system image** into `/tmp`
* fully releases the active image again
* keeps the System Partition mounted
* runs the updater entirely from RAM

---

## Final filesystem layout

After `pivot` finishes in the normal boot path, the system root is composed like this:

| Path            | Source           | Mode    |
| --------------- | ---------------- | ------- |
| `/System`       | SquashFS image   | RO      |
| `/usr`          | SquashFS image   | RO      |
| `/bin`          | SquashFS image   | RO      |
| `/sbin`         | SquashFS image   | RO      |
| `/Users`        | System Partition | RW      |
| `/Library`      | System Partition | RW      |
| `/private`      | System Partition | RW      |
| `/Volumes`      | System Partition | RW      |
| `/Applications` | System Partition | RW      |
| `/mnt/system`   | System Partition | RW      |
| `/dev`          | devtmpfs         | RW      |
| `/proc`         | procfs           | virtual |
| `/sys`          | sysfs            | virtual |
| `/run`          | tmpfs            | RW      |
| `/tmp`          | tmpfs            | RW      |

If persistent directories such as `/Users` or `/Library` do not exist yet on the System Partition, `pivot` creates them automatically.

Simplified view:

```
/
├── System        -> SquashFS (RO)
├── usr           -> SquashFS (RO)
├── bin           -> SquashFS (RO)
├── sbin          -> SquashFS (RO)
├── Users         -> System Partition (RW)
├── Library       -> System Partition (RW)
├── private       -> System Partition (RW)
├── Volumes       -> System Partition (RW)
├── Applications  -> System Partition (RW)
├── mnt/system    -> System Partition (RW)
├── dev           -> devtmpfs
├── proc          -> procfs
├── sys           -> sysfs
├── run           -> tmpfs
└── tmp           -> tmpfs
```

---

## Configuration

`pivot` expects this file inside the initramfs:

```
/pivot.config
```

Example:

```toml
[system]
mode = "installed"
active_slot = "A"

[hardware]
boot_partition_uuid   = "11111111-2222-3333-4444-555555555555"
system_partition_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"

[images]
slot_a = "base_system_a.img"
slot_b = "base_system_b.img"
```

### Fields

* `system.mode`

  * `installed`
  * `live`
    `live` is parsed but rejected by this binary.

* `system.active_slot`

  * `a`
  * `b`

* `hardware.boot_partition_uuid`

  * GPT PARTUUID of the Boot Partition

* `hardware.system_partition_uuid`

  * GPT PARTUUID of the System Partition

* `images.slot_a` / `images.slot_b`

  * image filenames stored under `/mnt/system/system/`

---

## Update mode

Update mode starts only if both files exist:

```
/mnt/system/private/system/StartUpdateInstaller
/mnt/system/var/update/sys_update.fbuimg
```

If both are present, `pivot` performs the following sequence:

1. mounts `/tmp` as tmpfs
2. copies the updater from the active system image

```
/system/base_root/usr/libexec/updateinstaller
```

to

```
/tmp/updateinstaller
```

3. unmounts all bind mounts referencing the active image

```
/system/rootfs/System
/system/rootfs/usr
/system/rootfs/bin
/system/rootfs/sbin
```

4. unmounts the SquashFS mount

```
/system/base_root
```

5. detaches the loop device backing the image
6. keeps the System Partition mounted at `/mnt/system`
7. executes the updater from RAM

```
/tmp/updateinstaller
```

### Runtime layout during update

After the active image is released, the system runs from a minimal RAM environment:

```
/
├── dev
├── proc
├── sys
├── tmp
│   └── updateinstaller
└── mnt/system
```

The updater then works directly on files stored on the System Partition:

```
/mnt/system/system/slot_a.img
/mnt/system/system/slot_b.img
/mnt/system/var/update/sys_update.fbuimg
```

---

## Logging and errors

`pivot` logs to:

* `/dev/kmsg`
* `stderr`

If a fatal error occurs, `pivot`:

1. logs the error
2. tries to trigger a kernel panic via `/proc/sysrq-trigger`
3. falls back to sending `SIGABRT`

This guarantees that PID 1 never silently exits.

---

## Root switch

`pivot` does **not** use `pivot_root(2)`.

Instead it performs the initramfs-safe sequence:

1. bind-mount the new root onto itself
2. `chdir()` into it
3. move it onto `/` using `MS_MOVE`
4. clean selected old initramfs files
5. `chroot(".")`
6. `exec()` the real init

---

## Runtime requirements

The initramfs must contain:

* the `pivot` binary
* `/pivot.config`
* `/sbin/e2fsck`
* empty directories for

```
/dev
/proc
/sys
```

The kernel must support:

* devtmpfs
* procfs
* sysfs
* loop devices
* SquashFS
* ext4
* GPT partitioning

The selected system image must contain:

```
/sbin/syscored
```

For update mode it must also contain:

```
/usr/libexec/updateinstaller
```

---

## Build

Typical static builds:

```bash
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

Example dependencies:

```toml
[dependencies]
loopdev = "0.4"
serde   = { version = "1", features = ["derive"] }
toml    = "0.8"
nix     = { version = "0.29", features = ["mount", "signal", "fs", "process"] }
```

---

## Summary

`pivot` is a small early-boot PID 1 that:

* discovers partitions directly from GPT
* checks and mounts the writable System Partition
* supports A/B SquashFS system images
* assembles the final root from read-only and writable parts
* supports RAM-based update execution
* loads the updater from the active system image
* fully releases the active image before starting the updater
* keeps the System Partition mounted during updates
* switches root without using `pivot_root(2)`
* hands off cleanly to `/sbin/syscored`
