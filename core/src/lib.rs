use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jboolean, jlong, jstring};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use tokio::time::timeout;

struct Engine {
    rt: Runtime,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    status: Arc<StdMutex<String>>,
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

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeInit(_env: JNIEnv, _class: JClass) -> jlong {
    init_logger();
    let rt = Runtime::new().expect("tokio runtime");
    let engine = Box::new(Engine {
        rt,
        handle: Mutex::new(None),
        status: Arc::new(StdMutex::new("未连接".to_string())),
    });
    Box::into_raw(engine) as jlong
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

    // Stage 1.5：把握手与数据通道结果写进可读状态，供界面轮询显示。
    // 这是独立的 hy2 连接，不经过 VpnService 的 TUN，也不转发 TUN 流量。
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
        engine
            .status
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "状态读取失败".to_string())
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
    unsafe {
        drop(Box::from_raw(ptr as *mut Engine));
    }
}
