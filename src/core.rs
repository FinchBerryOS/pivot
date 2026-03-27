use nix::fcntl::OFlag;
use nix::sys::signal::{kill, Signal};
use nix::unistd::getpid;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread::sleep;
use std::time::Duration;

static KMSG_FD: AtomicI32 = AtomicI32::new(-1);

pub fn kmsg_write(msg: &str) {
    let mut fd = KMSG_FD.load(Ordering::Relaxed);
    if fd < 0 {
        use std::os::unix::io::IntoRawFd;
        if let Ok(f) = fs::OpenOptions::new()
            .write(true)
            .custom_flags(OFlag::O_CLOEXEC.bits())
            .open("/dev/kmsg")
        {
            let new_fd = f.into_raw_fd();
            match KMSG_FD.compare_exchange(-1, new_fd, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    fd = new_fd;
                }
                Err(existing) => {
                    let _ = nix::unistd::close(new_fd);
                    fd = existing;
                }
            }
        }
    }
    if fd >= 0 {
        let _ = nix::unistd::write(fd, msg.as_bytes());
    }
}

#[macro_export]
macro_rules! klog {
    ($($arg:tt)*) => {{
        let msg = format!("[PIVOT] {}\n", format!($($arg)*));
        $crate::kmsg_write(&msg);
        eprint!("{}", msg);
    }};
}

pub fn fatal_error(msg: &str) -> ! {
    klog!("FATAL ERROR: {}", msg);
    let _ = std::io::stderr().flush();

    if let Ok(mut f) = fs::OpenOptions::new().write(true).open("/proc/sysrq-trigger") {
        let _ = f.write_all(b"c");
    }

    let _ = kill(getpid(), Signal::SIGABRT);

    loop {
        sleep(Duration::from_secs(1));
    }
}

pub fn is_container() -> bool {
    if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
        return true;
    }

    if std::env::var_os("container").is_some() {
        return true;
    }

    if let Ok(cgroup) = fs::read_to_string("/proc/1/cgroup") {
        let cg = cgroup.to_ascii_lowercase();
        if cg.contains("docker")
            || cg.contains("containerd")
            || cg.contains("podman")
            || cg.contains("kubepods")
            || cg.contains("lxc")
            || cg.contains("libpod")
        {
            return true;
        }
    }

    if let Ok(environ) = fs::read("/proc/1/environ") {
        let env = String::from_utf8_lossy(&environ).to_ascii_lowercase();
        if env.contains("container=") {
            return true;
        }
    }

    false
}

pub fn verify_container_requirements() {
    use nix::mount::{mount, umount2, MntFlags, MsFlags};
    use std::fs;
    use std::path::Path;

    let probe_root = Path::new("/tmp/pivot-cap-check");
    let bind_src = probe_root.join("src");
    let bind_dst = probe_root.join("dst");
    let tmpfs_dst = probe_root.join("tmpfs");

    fs::create_dir_all(&bind_src)
        .unwrap_or_else(|e| fatal_error(&format!("container check mkdir src: {}", e)));
    fs::create_dir_all(&bind_dst)
        .unwrap_or_else(|e| fatal_error(&format!("container check mkdir dst: {}", e)));
    fs::create_dir_all(&tmpfs_dst)
        .unwrap_or_else(|e| fatal_error(&format!("container check mkdir tmpfs: {}", e)));

    match mount(
        Some(bind_src.as_path()),
        bind_dst.as_path(),
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    ) {
        Ok(_) => {
            umount2(bind_dst.as_path(), MntFlags::MNT_DETACH)
                .unwrap_or_else(|e| fatal_error(&format!("container check umount bind: {}", e)));
        }
        Err(e) => fatal_error(&format!(
            "container lacks bind-mount capability: {}",
            e
        )),
    }

    match mount(
        Some("tmpfs"),
        tmpfs_dst.as_path(),
        Some("tmpfs"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(_) => {
            umount2(tmpfs_dst.as_path(), MntFlags::MNT_DETACH)
                .unwrap_or_else(|e| fatal_error(&format!("container check umount tmpfs: {}", e)));
        }
        Err(e) => fatal_error(&format!(
            "container lacks tmpfs mount capability: {}",
            e
        )),
    }

    if let Err(e) = fs::metadata("/dev/loop-control") {
        fatal_error(&format!(
            "container missing /dev/loop-control: {}",
            e
        ));
    }

    if !Path::new("/dev").exists() || !Path::new("/proc").exists() || !Path::new("/sys").exists() {
        fatal_error("container is missing /dev, /proc or /sys");
    }

    let _ = fs::remove_dir_all(probe_root);
}

pub fn system_source_root() -> &'static str {
    if is_container() {
        "/"
    } else {
        "/mnt/system"
    }
}