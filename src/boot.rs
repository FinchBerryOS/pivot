use fbppid_rs::register_broker;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::libc;
use nix::mount::{mount, MsFlags};
use nix::unistd::{chdir, chroot, close, fork, pipe, read, write, ForkResult};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::fatal_error;

pub fn setup_vfs() {
    if crate::core::is_container() {
        match mount(
            Some("devtmpfs"),
            "/dev",
            Some("devtmpfs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(_) => {}
            Err(nix::errno::Errno::EBUSY) | Err(nix::errno::Errno::EPERM) => {}
            Err(e) => fatal_error(&format!("Mount devtmpfs: {}", e)),
        }

        match mount(
            None::<&str>,
            "/proc",
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(_) => {}
            Err(nix::errno::Errno::EBUSY) | Err(nix::errno::Errno::EPERM) => {}
            Err(e) => fatal_error(&format!("Mount procfs: {}", e)),
        }

        match mount(
            None::<&str>,
            "/sys",
            Some("sysfs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(_) => {}
            Err(nix::errno::Errno::EBUSY) | Err(nix::errno::Errno::EPERM) => {}
            Err(e) => fatal_error(&format!("Mount sysfs: {}", e)),
        }

        return;
    }

    mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount devtmpfs: {}", e)));
    mount(
        None::<&str>,
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount procfs: {}", e)));
    mount(
        None::<&str>,
        "/sys",
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount sysfs: {}", e)));
}

fn setup_tmp(staging: &Path) {
    let path = staging.join("tmp");

    if crate::core::is_container() {
        match mount(
            Some("tmpfs"),
            &path,
            Some("tmpfs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(_) => {}
            Err(nix::errno::Errno::EBUSY) | Err(nix::errno::Errno::EPERM) => {
                klog!("WARN: tmpfs mount for /tmp unavailable in container, keeping existing /tmp");
            }
            Err(e) => fatal_error(&format!("Mount tmpfs for /tmp: {}", e)),
        }
    } else {
        mount(
            Some("tmpfs"),
            &path,
            Some("tmpfs"),
            MsFlags::empty(),
            None::<&str>,
        )
        .unwrap_or_else(|e| fatal_error(&format!("Mount tmpfs for /tmp: {}", e)));
    }

    fs::set_permissions(&path, fs::Permissions::from_mode(0o1777))
        .unwrap_or_else(|e| fatal_error(&format!("chmod 1777 /tmp: {}", e)));
}

pub fn move_vfs_to_new_root() {
    let staging = Path::new("/system/rootfs");

    if crate::core::is_container() {
        for vfs in &["dev", "proc", "sys"] {
            let source = format!("/{}", vfs);
            mount(
                Some(source.as_str()),
                &staging.join(vfs),
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )
            .unwrap_or_else(|e| fatal_error(&format!("Bind /{} into staging: {}", vfs, e)));
        }

        match mount(
            Some("tmpfs"),
            &staging.join("run"),
            Some("tmpfs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(_) => {}
            Err(nix::errno::Errno::EBUSY) | Err(nix::errno::Errno::EPERM) => {
                klog!("WARN: tmpfs mount for /run unavailable in container, keeping existing /run");
            }
            Err(e) => fatal_error(&format!("Mount tmpfs for /run: {}", e)),
        }

        setup_tmp(staging);
        return;
    }

    for vfs in &["dev", "proc", "sys"] {
        let source = format!("/{}", vfs);
        mount(
            Some(source.as_str()),
            &staging.join(vfs),
            None::<&str>,
            MsFlags::MS_MOVE,
            None::<&str>,
        )
        .unwrap_or_else(|e| fatal_error(&format!("MS_MOVE /{} into staging: {}", vfs, e)));
    }

    mount(
        Some("tmpfs"),
        &staging.join("run"),
        Some("tmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Mount tmpfs for /run: {}", e)));

    setup_tmp(staging);
}

fn free_initramfs(active_mounts: &HashSet<String>) {
    if !Path::new("/pivot.config").exists() {
        klog!(
            "WARN: free_initramfs: /pivot.config missing at '/' – \
               root may already be switched, skipping cleanup to avoid data loss"
        );
        return;
    }

    let candidates: &[&str] = &[
        "/init",
        "/pivot.config",
        "/dev",
        "/proc",
        "/sys",
        "/bin",
        "/sbin",
        "/usr",
        "/lib",
        "/lib64",
        "/etc",
    ];

    for path_str in candidates {
        if active_mounts.contains(*path_str) {
            klog!("free_initramfs: skipping active mountpoint {}", path_str);
            continue;
        }
        let path = Path::new(path_str);

        let result = fs::remove_dir(path).or_else(|_| fs::remove_file(path));
        match result {
            Ok(_) => klog!("free_initramfs: removed {}", path_str),
            Err(e) => klog!("free_initramfs: skipped {} ({})", path_str, e),
        }
    }
}

pub fn perform_pivot_and_exec() -> ! {
    let new_root = "/system/rootfs";

    mount(
        Some(new_root),
        new_root,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .unwrap_or_else(|e| fatal_error(&format!("Bind new_root onto itself: {}", e)));

    chdir(new_root).unwrap_or_else(|e| fatal_error(&format!("chdir to new_root: {}", e)));

    let active_mounts: HashSet<String> = fs::read_to_string("/proc/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(|s| s.to_string()))
        .collect();

    if !crate::core::is_container() {
        mount(
            Some(new_root),
            "/",
            None::<&str>,
            MsFlags::MS_MOVE,
            None::<&str>,
        )
        .unwrap_or_else(|e| fatal_error(&format!("MS_MOVE new_root onto /: {}", e)));

        free_initramfs(&active_mounts);
    }

    chroot(".").unwrap_or_else(|e| fatal_error(&format!("chroot to new root: {}", e)));

    chdir("/").unwrap_or_else(|e| fatal_error(&format!("chdir to / in new root: {}", e)));

    {
        use std::os::unix::io::IntoRawFd;

        let console = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/console")
            .or_else(|console_err| {
                if crate::core::is_container() {
                    klog!(
                        "WARN: /dev/console unavailable in container ({}), falling back to /dev/null",
                        console_err
                    );
                    fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/dev/null")
                } else {
                    Err(console_err)
                }
            })
            .unwrap_or_else(|e| fatal_error(&format!("Open console fallback failed: {}", e)));

        let fd = console.into_raw_fd();

        for target_fd in 0i32..=2 {
            if fd != target_fd {
                let rc = unsafe { libc::dup2(fd, target_fd) };
                if rc == -1 {
                    fatal_error(&format!(
                        "dup2 console→fd{}: {}",
                        target_fd,
                        std::io::Error::last_os_error()
                    ));
                }
            }
        }

        if fd > 2 {
            close(fd).unwrap_or_else(|e| fatal_error(&format!("close console fd: {}", e)));
        }
    }

    {
        let term = std::env::var("TERM").unwrap_or_else(|_| "linux".to_string());
        let keys: Vec<std::ffi::OsString> = std::env::vars_os().map(|(k, _)| k).collect();
        unsafe {
            for k in keys {
                std::env::remove_var(&k);
            }
            std::env::set_var("TERM", &term);
            std::env::set_var(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            );
        }
    }

    prepare_and_execute_fbportscore_engine();

    klog!("Executing /sbin/syscored ...");
    let err = Command::new("/sbin/syscored").exec();
    fatal_error(&format!("exec /sbin/syscored failed: {}", err));
}

fn prepare_and_execute_fbportscore_engine() {
    const READY_FD: i32 = 3;

    let (ready_read, ready_write) =
        pipe().unwrap_or_else(|e| fatal_error(&format!("pipe() ready: {}", e)));

    let (sync_read, sync_write) =
        pipe().unwrap_or_else(|e| fatal_error(&format!("pipe() sync: {}", e)));

    fcntl(ready_read.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .unwrap_or_else(|e| fatal_error(&format!("fcntl ready_read O_CLOEXEC: {}", e)));

    fcntl(sync_write.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .unwrap_or_else(|e| fatal_error(&format!("fcntl sync_write O_CLOEXEC: {}", e)));

    if ready_write.as_raw_fd() != READY_FD {
        let rc = unsafe { libc::dup2(ready_write.as_raw_fd(), READY_FD) };
        if rc == -1 {
            fatal_error(&format!(
                "dup2 ready_write→fd{}: {}",
                READY_FD,
                std::io::Error::last_os_error()
            ));
        }

        let rc = unsafe { libc::close(ready_write.as_raw_fd()) };
        if rc == -1 {
            fatal_error(&format!(
                "close original ready_write fd {}: {}",
                ready_write.as_raw_fd(),
                std::io::Error::last_os_error()
            ));
        }
    }

    fcntl(READY_FD, FcntlArg::F_SETFD(FdFlag::empty()))
        .unwrap_or_else(|e| fatal_error(&format!("fcntl fd{} clear O_CLOEXEC: {}", READY_FD, e)));

    klog!(
        "Forking /usr/libexec/fbportscore (ready pipe on FD {}, broker registration before exec)...",
        READY_FD
    );

    match unsafe { fork() }.unwrap_or_else(|e| fatal_error(&format!("fork(): {}", e))) {
        ForkResult::Child => {
            let rc = unsafe { libc::close(ready_read.as_raw_fd()) };
            if rc == -1 {
                fatal_error(&format!(
                    "close ready_read in child: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let rc = unsafe { libc::close(sync_write.as_raw_fd()) };
            if rc == -1 {
                fatal_error(&format!(
                    "close sync_write in child: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let mut byte = [0u8; 1];
            match read(sync_read.as_raw_fd(), &mut byte) {
                Ok(1) => {}
                Ok(0) => fatal_error("sync pipe closed before broker registration completed"),
                Ok(n) => fatal_error(&format!("sync pipe: unexpected read length {}", n)),
                Err(e) => fatal_error(&format!("read() on sync pipe: {}", e)),
            }

            let rc = unsafe { libc::close(sync_read.as_raw_fd()) };
            if rc == -1 {
                fatal_error(&format!(
                    "close sync_read in child: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let err = Command::new("/usr/libexec/fbportscore").exec();
            eprintln!("[PIVOT] exec /usr/libexec/fbportscore failed: {}", err);
            std::process::exit(1);
        }

        ForkResult::Parent { child } => {
            let rc = unsafe { libc::close(READY_FD) };
            if rc == -1 {
                fatal_error(&format!(
                    "close fd{} in parent: {}",
                    READY_FD,
                    std::io::Error::last_os_error()
                ));
            }

            let rc = unsafe { libc::close(sync_read.as_raw_fd()) };
            if rc == -1 {
                fatal_error(&format!(
                    "close sync_read in parent: {}",
                    std::io::Error::last_os_error()
                ));
            }

            klog!("Registering fbportscore broker pid {}...", child);

            register_broker(child.as_raw())
                .unwrap_or_else(|e| fatal_error(&format!("register_broker({}): {}", child, e)));

            write(sync_write.as_raw_fd(), &[1])
                .unwrap_or_else(|e| fatal_error(&format!("write() on sync pipe: {}", e)));

            let rc = unsafe { libc::close(sync_write.as_raw_fd()) };
            if rc == -1 {
                fatal_error(&format!(
                    "close sync_write in parent: {}",
                    std::io::Error::last_os_error()
                ));
            }

            klog!("Waiting for fbportscore ready signal (pid {})...", child);

            let mut ready_byte = [0u8; 1];
            match read(ready_read.as_raw_fd(), &mut ready_byte) {
                Ok(1) => klog!(
                    "fbportscore signalled ready (byte=0x{:02x}), continuing",
                    ready_byte[0]
                ),
                Ok(0) => fatal_error("fbportscore closed ready pipe without writing – crashed?"),
                Ok(n) => fatal_error(&format!("ready pipe: unexpected read length {}", n)),
                Err(e) => fatal_error(&format!("read() on ready pipe: {}", e)),
            }

            let rc = unsafe { libc::close(ready_read.as_raw_fd()) };
            if rc == -1 {
                fatal_error(&format!(
                    "close ready_read in parent: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
}