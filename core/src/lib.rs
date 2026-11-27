use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jboolean, jint, jlong, jstring};
use std::sync::{Arc, Mutex as StdMutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use tokio::time::timeout;

struct TunHandle {
    stop: Arc<AtomicBool>,
    packets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

struct Engine {
    rt: Runtime,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    status: Arc<StdMutex<String>>,
    tun: StdMutex<Option<TunHandle>>,
}

fn set_status(status: &Arc<StdMutex<String>>, text: impl Into<String>) {
    if let Ok(mut guard) = status.lock() {
        *guard = text.into();
    }
}

fn init_logger() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("hy2core"),
    );
}

// Stage 2 · Step 1：只从 TUN fd 读原始 IP 包并计数，不做任何转发。
// 用独立 OS 线程 + poll(200ms)；fd 设 O_NONBLOCK，退出时由本函数负责 close。
fn tun_reader(fd: i32, stop: Arc<AtomicBool>, packets: Arc<AtomicU64>, bytes: Arc<AtomicU64>) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    let mut buf = [0u8; 65536];
    log::info!("tun reader started fd={}", fd);

    while !stop.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1 as libc::nfds_t, 200) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("tun poll failed: {}", e);
            break;
        }
        if rc == 0 {
            continue; // 200ms 超时，回到循环检查 stop
        }
        if (pfd.revents & libc::POLLIN) == 0 {
            continue;
        }

        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            let total = packets.fetch_add(1, Ordering::Relaxed) + 1;
            bytes.fetch_add(n as u64, Ordering::Relaxed);
            if total == 1 || total % 20 == 0 {
                let ipver = buf[0] >> 4;
                let proto = if ipver == 4 && n as usize > 9 { buf[9] } else { 0 };
                log::info!(
                    "tun packet #{} len={} ipver={} proto={}",
                    total, n, ipver, proto
                );
            }
        } else if n == 0 {
            break;
        } else {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            log::error!("tun read failed: {}", e);
            break;
        }
    }

    unsafe {
        libc::close(fd);
    }
    log::info!("tun reader stopped fd={}", fd);
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeInit(_env: JNIEnv, _class: JClass) -> jlong {
    init_logger();
    let rt = Runtime::new().expect("tokio runtime");
    let engine = Box::new(Engine {
        rt,
        handle: Mutex::new(None),
        status: Arc::new(StdMutex::new("未连接".to_string())),
        tun: StdMutex::new(None),
    });
    Box::into_raw(engine) as jlong
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeAttachTun(
    _env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    fd: jint,
) -> jboolean {
    if ptr == 0 || fd < 0 {
        return false as jboolean;
    }
    let engine = unsafe { &*(ptr as *mut Engine) };

    let stop = Arc::new(AtomicBool::new(false));
    let packets = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let (s, p, b) = (stop.clone(), packets.clone(), bytes.clone());
    let raw_fd = fd as i32;
    std::thread::spawn(move || tun_reader(raw_fd, s, p, b));

    if let Ok(mut guard) = engine.tun.lock() {
        if let Some(old) = guard.take() {
            old.stop.store(true, Ordering::Relaxed);
        }
        *guard = Some(TunHandle {
            stop,
            packets,
            bytes,
        });
    }
    true as jboolean
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeDetachTun(
    _env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) {
    if ptr == 0 {
        return;
    }
    let engine = unsafe { &*(ptr as *mut Engine) };
    if let Ok(mut guard) = engine.tun.lock() {
        if let Some(h) = guard.take() {
            h.stop.store(true, Ordering::Relaxed);
        }
    }
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    server: jni::objects::JString,
    auth: jni::objects::JString,
) -> jboolean {
    let server: String = match env.get_string(&server) {
        Ok(s) => s.into(),
        Err(_) => return false as jboolean,
    };
    let auth: String = match env.get_string(&auth) {
        Ok(s) => s.into(),
        Err(_) => return false as jboolean,
    };
    if ptr == 0 {
        return false as jboolean;
    }
    let engine = unsafe { &*(ptr as *mut Engine) };

    let cfg = rsteria2::Config {
        server_addr: server,
        auth,
        ..Default::default()
    };

    let status = engine.status.clone();
    set_status(&status, "正在握手…");

    // 握手是独立连接，不经过 TUN。Step 2 起会把 TUN 流量转发到这里。
    let task = engine.rt.spawn(async move {
        let handshake = timeout(Duration::from_secs(15), rsteria2::connect(&cfg)).await;
        let client = match handshake {
            Ok(Ok(c)) => {
                log::info!("hy2 handshake ok");
                set_status(&status, "握手成功 · 正在验证数据通道…");
                c
            }
            Ok(Err(e)) => {
                log::error!("hy2 handshake failed: {}", e);
                set_status(&status, format!("握手失败：{}", e));
                return;
            }
            Err(_) => {
                log::error!("hy2 handshake timeout");
                set_status(&status, "握手失败：15 秒超时（地址/端口/网络不通）");
                return;
            }
        };

        let stream = timeout(Duration::from_secs(10), client.tcp_connect("ifconfig.me:80")).await;
        match stream {
            Ok(Ok(mut s)) => {
                use tokio::io::AsyncWriteExt;
                match s
                    .write_all(b"GET / HTTP/1.0\r\nHost: ifconfig.me\r\n\r\n")
                    .await
                {
                    Ok(_) => {
                        log::info!("hy2 tcp channel ok");
                        set_status(&status, "隧道通：握手成功，TCP 数据通道可用");
                    }
                    Err(e) => {
                        log::error!("hy2 write failed: {}", e);
                        set_status(&status, format!("握手成功，但写数据失败：{}", e));
                    }
                }
            }
            Ok(Err(e)) => {
                log::error!("tcp_connect failed: {}", e);
                set_status(&status, format!("握手成功，但 TCP 连接失败：{}", e));
            }
            Err(_) => {
                log::error!("tcp_connect timeout");
                set_status(&status, "握手成功，但 TCP 连接超时");
            }
        }
    });

    let mut guard = engine.handle.blocking_lock();
    *guard = Some(task);
    true as jboolean
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeGetStatus(
    env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jstring {
    let text = if ptr == 0 {
        "引擎未初始化".to_string()
    } else {
        let engine = unsafe { &*(ptr as *mut Engine) };
        let base = engine
            .status
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "状态读取失败".to_string());

        let tun_info = if let Ok(g) = engine.tun.lock() {
            match g.as_ref() {
                Some(h) => format!(
                    "\nTUN fd：已收到 {} 个 IP 包 / {} 字节",
                    h.packets.load(Ordering::Relaxed),
                    h.bytes.load(Ordering::Relaxed)
                ),
                None => "\nTUN fd：未挂载".to_string(),
            }
        } else {
            "\nTUN fd：状态锁异常".to_string()
        };
        format!("{}{}", base, tun_info)
    };
    match env.new_string(text) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeStop(_env: JNIEnv, _class: JClass, ptr: jlong) {
    if ptr == 0 {
        return;
    }
    let engine = unsafe { &*(ptr as *mut Engine) };
    let mut guard = engine.handle.blocking_lock();
    if let Some(h) = guard.take() {
        h.abort();
    }
    set_status(&engine.status, "已断开");
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeDestroy(_env: JNIEnv, _class: JClass, ptr: jlong) {
    if ptr == 0 {
        return;
    }
    let engine = unsafe { &*(ptr as *mut Engine) };
    if let Ok(mut guard) = engine.tun.lock() {
        if let Some(h) = guard.take() {
            h.stop.store(true, Ordering::Relaxed);
        }
    }
    unsafe {
        drop(Box::from_raw(ptr as *mut Engine));
    }
}
