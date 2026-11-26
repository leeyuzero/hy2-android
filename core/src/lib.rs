use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jboolean, jlong};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

struct Engine {
    rt: Runtime,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
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

    // Stage 1: 仅验证握手 + 单次 TCP 请求。
    // Stage 2 将替换为常驻 SOCKS5 入站，把 TUN 流量转发进来。
    let task = engine.rt.spawn(async move {
        match rsteria2::connect(&cfg).await {
            Ok(client) => {
                log::info!("hy2 handshake ok");
                match client.tcp_connect("ifconfig.me:80").await {
                    Ok(mut s) => {
                        use tokio::io::AsyncWriteExt;
                        let _ = s.write_all(b"GET / HTTP/1.0\r\nHost: ifconfig.me\r\n\r\n").await;
                        log::info!("hy2 simple request dispatched (Stage 1 smoke test)");
                    }
                    Err(e) => log::error!("tcp_connect failed: {}", e),
                }
            }
            Err(e) => log::error!("hy2 handshake failed: {}", e),
        }
    });

    let mut guard = engine.handle.blocking_lock();
    *guard = Some(task);
    true as jboolean
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
