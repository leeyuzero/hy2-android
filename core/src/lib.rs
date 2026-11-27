use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jboolean, jint, jlong, jstring};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex as StdMutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address};

const TUN_ADDR: [u8; 4] = [10, 0, 0, 2];
const VIRTUAL_DNS: [u8; 4] = [10, 0, 0, 1];
const DNS_UPSTREAM: &str = "8.8.8.8:53";
const MTU: usize = 1500;
const SOCK_BUF: usize = 32768;

#[derive(Clone)]
struct TunHandle {
    stop: Arc<AtomicBool>,
    packets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    conns: Arc<AtomicU64>,
    dns: Arc<AtomicU64>,
}

struct Engine {
    rt: Runtime,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    status: Arc<StdMutex<String>>,
    tun: StdMutex<Option<TunHandle>>,
    client: StdMutex<Option<Arc<rsteria2::ReconnectableClient>>>,
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

// ---------- 原始 IP 包解析（只用于分流，不改动包内容） ----------

// 提取「新 SYN」的目标端口：smoltcp 的 listen() 不允许 port=0，
// 必须按目标端口动态创建监听 socket，因此要在包进协议栈前先看到端口。
fn syn_port(buf: &[u8]) -> Option<u16> {
    if buf.len() < 20 || buf[0] >> 4 != 4 {
        return None;
    }
    let ihl = ((buf[0] & 0x0f) as usize) * 4;
    if ihl < 20 || buf.len() < ihl + 20 || buf[9] != 6 {
        return None;
    }
    let t = &buf[ihl..];
    let flags = t[13];
    if flags & 0x02 != 0 && flags & 0x10 == 0 {
        Some(u16::from_be_bytes([t[2], t[3]]))
    } else {
        None
    }
}

struct DnsReq {
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    payload: Vec<u8>,
}

// 识别发往虚拟 DNS 的 UDP 53 查询，直接交给 DNS worker，不进 smoltcp
fn parse_dns(buf: &[u8]) -> Option<DnsReq> {
    if buf.len() < 28 || buf[0] >> 4 != 4 {
        return None;
    }
    let ihl = ((buf[0] & 0x0f) as usize) * 4;
    if ihl < 20 || buf.len() < ihl + 8 || buf[9] != 17 {
        return None;
    }
    let mut src_ip = [0u8; 4];
    src_ip.copy_from_slice(&buf[12..16]);
    let mut dst_ip = [0u8; 4];
    dst_ip.copy_from_slice(&buf[16..20]);
    let u = &buf[ihl..];
    let src_port = u16::from_be_bytes([u[0], u[1]]);
    let dst_port = u16::from_be_bytes([u[2], u[3]]);
    if dst_ip != VIRTUAL_DNS || dst_port != 53 {
        return None;
    }
    let ulen = u16::from_be_bytes([u[4], u[5]]) as usize;
    if ulen < 8 || ihl + ulen > buf.len() {
        return None;
    }
    Some(DnsReq {
        src_ip,
        dst_ip,
        src_port,
        payload: buf[ihl + 8..ihl + ulen].to_vec(),
    })
}

fn ip_checksum(h: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < h.len() {
        sum += u16::from_be_bytes([h[i], h[i + 1]]) as u32;
        i += 2;
    }
    if i < h.len() {
        sum += (h[i] as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

// 手工构造 IPv4 + UDP 应答写回 TUN（IPv4 的 UDP 校验和可为 0）
fn build_dns_reply(req: &DnsReq, payload: &[u8]) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let total = 20 + udp_len;
    let mut p = vec![0u8; total];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // Don't Fragment
    p[8] = 64;
    p[9] = 17; // UDP
    p[12..16].copy_from_slice(&req.dst_ip);
    p[16..20].copy_from_slice(&req.src_ip);
    p[20..22].copy_from_slice(&53u16.to_be_bytes());
    p[22..24].copy_from_slice(&req.src_port.to_be_bytes());
    p[24..26].copy_from_slice(&(udp_len as u16).to_be_bytes());
    p[28..].copy_from_slice(payload);
    let c = ip_checksum(&p[..20]);
    p[10..12].copy_from_slice(&c.to_be_bytes());

    // UDP 校验和（IPv4 伪首部）：部分 ROM 内核会校验，置 0 可能被拒收
    let mut pseudo = Vec::with_capacity(12 + udp_len);
    pseudo.extend_from_slice(&req.dst_ip);
    pseudo.extend_from_slice(&req.src_ip);
    pseudo.extend_from_slice(&[0, 17]);
    pseudo.extend_from_slice(&(udp_len as u16).to_be_bytes());
    pseudo.extend_from_slice(&p[20..]);
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < pseudo.len() {
        sum += u16::from_be_bytes([pseudo[i], pseudo[i + 1]]) as u32;
        i += 2;
    }
    if i < pseudo.len() {
        sum += (pseudo[i] as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let uc = !(sum as u16);
    p[26..28].copy_from_slice(&(if uc == 0 { 0xffff } else { uc }).to_be_bytes());
    p
}

// ---------- smoltcp Device：包装 TUN fd ----------

struct Shared {
    cache: VecDeque<Vec<u8>>,      // 等监听 socket 建好再投递的包
    new_ports: VecDeque<u16>,      // 需要新建监听的端口
    known_ports: HashSet<u16>,     // 已见过（已建过监听）的端口
    dns_reqs: VecDeque<DnsReq>,
}

struct TunDevice {
    fd: i32,
    shared: Rc<RefCell<Shared>>,
    packets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

struct MyRxToken {
    buffer: Vec<u8>,
}
impl phy::RxToken for MyRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}

struct MyTxToken {
    fd: i32,
}
impl phy::TxToken for MyTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        unsafe {
            libc::write(self.fd, buffer.as_ptr() as *const libc::c_void, buffer.len());
        }
        result
    }
}

impl Device for TunDevice {
    type RxToken<'a> = MyRxToken;
    type TxToken<'a> = MyTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = MTU;
        caps.medium = Medium::Ip;
        caps
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        {
            let mut sh = self.shared.borrow_mut();
            if let Some(pkt) = sh.cache.pop_front() {
                return Some((MyRxToken { buffer: pkt }, MyTxToken { fd: self.fd }));
            }
        }

        let mut buf = vec![0u8; 65536];
        let n = unsafe {
            libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n <= 0 {
            return None; // O_NONBLOCK 下无数据
        }
        buf.truncate(n as usize);
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);

        if let Some(req) = parse_dns(&buf) {
            self.shared.borrow_mut().dns_reqs.push_back(req);
            return None; // 不交给 smoltcp，由 DNS worker 处理
        }

        if let Some(port) = syn_port(&buf) {
            let mut sh = self.shared.borrow_mut();
            if !sh.known_ports.contains(&port) {
                // 缓存该 SYN，本轮返回 None；外层循环先建好监听 socket，
                // 下一轮再投递，避免被 smoltcp 回 RST 导致连接直接失败。
                sh.new_ports.push_back(port);
                sh.cache.push_back(buf);
                return None;
            }
        }

        Some((MyRxToken { buffer: buf }, MyTxToken { fd: self.fd }))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(MyTxToken { fd: self.fd })
    }
}

// ---------- 异步侧：TCP relay 与 DNS over TCP ----------

struct Conn {
    to_remote: mpsc::Sender<Vec<u8>>,
    from_remote: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

async fn tcp_relay(
    stream: rsteria2::DuplexStream,
    mut to_rx: mpsc::Receiver<Vec<u8>>,
    from_tx: mpsc::Sender<Vec<u8>>,
) {
    let (mut r, mut w) = tokio::io::split(stream);
    let mut buf = vec![0u8; 16384];
    loop {
        tokio::select! {
            item = to_rx.recv() => match item {
                Some(data) => {
                    if w.write_all(&data).await.is_err() { break; }
                }
                None => break,
            },
            res = r.read(&mut buf) => match res {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if from_tx.send(buf[..n].to_vec()).await.is_err() { break; }
                }
            },
        }
    }
}

async fn dns_worker(
    client: Arc<rsteria2::ReconnectableClient>,
    fd: i32,
    dns_counter: Arc<AtomicU64>,
    mut rx: mpsc::Receiver<DnsReq>,
) {
    let mut stream: Option<rsteria2::DuplexStream> = None;
    while let Some(req) = rx.recv().await {
        dns_counter.fetch_add(1, Ordering::Relaxed);
        let mut done = false;
        for _ in 0..2 {
            if stream.is_none() {
                stream = match timeout(
                    Duration::from_secs(10),
                    client.tcp_connect(DNS_UPSTREAM),
                )
                .await
                {
                    Ok(Ok(s)) => Some(s),
                    Ok(Err(e)) => {
                        log::error!("dns upstream connect failed: {}", e);
                        break;
                    }
                    Err(_) => {
                        log::error!("dns upstream connect timeout");
                        break;
                    }
                };
            }
            let s = match stream.as_mut() {
                Some(s) => s,
                None => break,
            };
            let len = req.payload.len() as u16;
            if s.write_all(&len.to_be_bytes()).await.is_err()
                || s.write_all(&req.payload).await.is_err()
            {
                stream = None;
                continue;
            }
            let mut lb = [0u8; 2];
            match timeout(Duration::from_secs(8), s.read_exact(&mut lb)).await {
                Ok(Ok(_)) => {}
                _ => {
                    stream = None;
                    continue;
                }
            }
            let n = u16::from_be_bytes(lb) as usize;
            if n == 0 || n > 4096 {
                stream = None;
                continue;
            }
            let mut resp = vec![0u8; n];
            match timeout(Duration::from_secs(8), s.read_exact(&mut resp)).await {
                Ok(Ok(_)) => {}
                _ => {
                    stream = None;
                    continue;
                }
            }
            let pkt = build_dns_reply(&req, &resp);
            unsafe {
                libc::write(fd, pkt.as_ptr() as *const libc::c_void, pkt.len());
            }
            done = true;
            break;
        }
        if !done {
            log::warn!("dns query dropped (upstream unavailable)");
        }
    }
}

// ---------- 同步侧：smoltcp 主循环（独占一个 OS 线程） ----------

fn set_nonblocking(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn run_stack(
    fd: i32,
    client: Arc<rsteria2::ReconnectableClient>,
    rt: Handle,
    tun: TunHandle,
) {
    set_nonblocking(fd);
    log::info!("stack started fd={}", fd);

    let shared = Rc::new(RefCell::new(Shared {
        cache: VecDeque::new(),
        new_ports: VecDeque::new(),
        known_ports: HashSet::new(),
        dns_reqs: VecDeque::new(),
    }));
    let (dns_tx, dns_rx) = mpsc::channel::<DnsReq>(64);
    let dns_counter = tun.dns.clone();
    let dns_client = client.clone();
    rt.spawn(async move {
        dns_worker(dns_client, fd, dns_counter, dns_rx).await;
    });

    let mut device = TunDevice {
        fd,
        shared: shared.clone(),
        packets: tun.packets.clone(),
        bytes: tun.bytes.clone(),
    };

    let mut config = IfaceConfig::new(HardwareAddress::Ip);
    config.random_seed = 0x5eed_1234;
    let mut iface = Interface::new(config, &mut device, Instant::now());
    iface.set_any_ip(true);
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(
            IpAddress::v4(TUN_ADDR[0], TUN_ADDR[1], TUN_ADDR[2], TUN_ADDR[3]),
            32,
        ));
    });
    let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(
        TUN_ADDR[0],
        TUN_ADDR[1],
        TUN_ADDR[2],
        TUN_ADDR[3],
    ));

    let mut sockets = SocketSet::new(vec![]);
    let mut conns: HashMap<SocketHandle, Conn> = HashMap::new();
    let mut buf = vec![0u8; 16384];

    while !tun.stop.load(Ordering::Relaxed) {
        // 1) 取走 Device 攒下的新端口与 DNS 请求（只登记端口，监听 socket 由第 2 步统一补齐）
        let mut need_listen: Vec<u16> = Vec::new();
        {
            let mut sh = shared.borrow_mut();
            while let Some(p) = sh.new_ports.pop_front() {
                sh.known_ports.insert(p);
            }
            while let Some(req) = sh.dns_reqs.pop_front() {
                if dns_tx.try_send(req).is_err() {
                    log::warn!("dns queue full, query dropped");
                }
            }
        }

        // 2) 每个已见端口至少保留一个处于 Listen 状态的 socket（无 backlog，必须补）
        let handles: Vec<SocketHandle> = sockets.iter().map(|(h, _)| h).collect();
        let mut listening: HashSet<u16> = HashSet::new();
        for h in &handles {
            let s = sockets.get::<tcp::Socket>(*h);
            if s.state() == tcp::State::Listen {
                listening.insert(s.listen_endpoint().port);
            }
        }
        {
            let sh = shared.borrow();
            for p in sh.known_ports.iter().copied() {
                if !listening.contains(&p) {
                    need_listen.push(p);
                }
            }
        }
        for port in need_listen.drain(..) {
            let rx = tcp::SocketBuffer::new(vec![0u8; SOCK_BUF]);
            let tx = tcp::SocketBuffer::new(vec![0u8; SOCK_BUF]);
            let mut s = tcp::Socket::new(rx, tx);
            if s.listen(port).is_ok() {
                sockets.add(s);
            }
        }

        // 3) 驱动协议栈（读写 TUN fd、处理 TCP 状态机）
        iface.poll(Instant::now(), &mut device, &mut sockets);

        // 4) 为新建立的连接起 hy2 TCP 转发任务
        let handles2: Vec<SocketHandle> = sockets.iter().map(|(h, _)| h).collect();
        let mut new_conns: Vec<(SocketHandle, String)> = Vec::new();
        let mut to_remove: Vec<SocketHandle> = Vec::new();
        for h in handles2 {
            let s = sockets.get_mut::<tcp::Socket>(h);
            match s.state() {
                tcp::State::Listen => continue,
                tcp::State::Closed | tcp::State::TimeWait => {
                    to_remove.push(h);
                    continue;
                }
                _ => {}
            }
            if !conns.contains_key(&h) {
                if let Some(ep) = s.local_endpoint() {
                    // any_ip 下 local_endpoint 就是 App 想要访问的真实目标
                    new_conns.push((h, format!("{}", ep)));
                }
            }
        }

        for (h, target) in new_conns {
            let (to_tx, to_rx) = mpsc::channel::<Vec<u8>>(64);
            let (from_tx, from_rx) = mpsc::channel::<Vec<u8>>(64);
            conns.insert(
                h,
                Conn {
                    to_remote: to_tx,
                    from_remote: from_rx,
                    pending: Vec::new(),
                },
            );
            let c = client.clone();
            let t = target.clone();
            rt.spawn(async move {
                match timeout(Duration::from_secs(15), c.tcp_connect(&t)).await {
                    Ok(Ok(s)) => tcp_relay(s, to_rx, from_tx).await,
                    Ok(Err(e)) => log::error!("tcp_connect {} failed: {}", t, e),
                    Err(_) => log::error!("tcp_connect {} timeout", t),
                }
            });
            log::info!("new tcp conn -> {}", target);
        }

        // 5) 双向搬运 socket <-> hy2 stream
        for (h, conn) in conns.iter_mut() {
            let s = sockets.get_mut::<tcp::Socket>(*h);
            // 先确认 channel 有空位再从 socket 取数据，避免取出来却发不出去导致丢包
            while s.can_recv() {
                match conn.to_remote.try_reserve() {
                    Ok(permit) => match s.recv_slice(&mut buf) {
                        Ok(n) if n > 0 => {
                            permit.send(buf[..n].to_vec());
                        }
                        _ => break,
                    },
                    Err(_) => break,
                }
            }
            if conn.pending.is_empty() {
                if let Ok(d) = conn.from_remote.try_recv() {
                    conn.pending = d;
                }
            }
            if !conn.pending.is_empty() {
                match s.send_slice(&conn.pending) {
                    Ok(n) => {
                        conn.pending.drain(..n);
                    }
                    Err(_) => {
                        s.close();
                    }
                }
            }
        }

        for h in to_remove.drain(..) {
            conns.remove(&h);
            sockets.remove(h);
        }
        tun.conns.store(conns.len() as u64, Ordering::Relaxed);

        // 6) 最多等 20ms 的 fd 可读，兼作 channel 数据的轮询间隔
        unsafe {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            libc::poll(&mut pfd as *mut libc::pollfd, 1 as libc::nfds_t, 20);
        }
    }

    unsafe {
        libc::close(fd);
    }
    log::info!("stack stopped fd={}", fd);
}

// ---------- JNI ----------

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeInit(_env: JNIEnv, _class: JClass) -> jlong {
    init_logger();
    let rt = Runtime::new().expect("tokio runtime");
    let engine = Box::new(Engine {
        rt,
        handle: Mutex::new(None),
        status: Arc::new(StdMutex::new("未连接".to_string())),
        tun: StdMutex::new(None),
        client: StdMutex::new(None),
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
    // 数据面与握手验证共用同一个可重连客户端，连接只建一次
    let rc = Arc::new(rsteria2::ReconnectableClient::new(cfg));
    if let Ok(mut g) = engine.client.lock() {
        *g = Some(rc.clone());
    }

    let status = engine.status.clone();
    set_status(&status, "正在握手…");
    let task = engine.rt.spawn(async move {
        let probe = timeout(Duration::from_secs(15), rc.tcp_connect("ifconfig.me:80")).await;
        match probe {
            Ok(Ok(mut s)) => {
                match s
                    .write_all(b"GET / HTTP/1.0\r\nHost: ifconfig.me\r\n\r\n")
                    .await
                {
                    Ok(_) => {
                        log::info!("hy2 handshake + tcp channel ok");
                        set_status(&status, "隧道通：握手成功，TCP 数据通道可用");
                    }
                    Err(e) => set_status(&status, format!("握手成功，但写数据失败：{}", e)),
                }
            }
            Ok(Err(e)) => {
                log::error!("hy2 handshake failed: {}", e);
                set_status(&status, format!("握手失败：{}", e));
            }
            Err(_) => {
                log::error!("hy2 handshake timeout");
                set_status(&status, "握手失败：15 秒超时（地址/端口/网络不通）");
            }
        }
    });
    let mut guard = engine.handle.blocking_lock();
    *guard = Some(task);
    true as jboolean
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
    let client = match engine.client.lock().ok().and_then(|g| g.as_ref().cloned()) {
        Some(c) => c,
        None => {
            set_status(&engine.status, "数据面启动失败：请先调用 nativeStart 建立客户端");
            return false as jboolean;
        }
    };

    let handle = TunHandle {
        stop: Arc::new(AtomicBool::new(false)),
        packets: Arc::new(AtomicU64::new(0)),
        bytes: Arc::new(AtomicU64::new(0)),
        conns: Arc::new(AtomicU64::new(0)),
        dns: Arc::new(AtomicU64::new(0)),
    };
    let thread_handle = handle.clone();
    let rt_handle = engine.rt.handle().clone();
    let raw_fd = fd as i32;
    std::thread::spawn(move || run_stack(raw_fd, client, rt_handle, thread_handle));

    if let Ok(mut guard) = engine.tun.lock() {
        if let Some(old) = guard.take() {
            old.stop.store(true, Ordering::Relaxed);
        }
        *guard = Some(handle);
    }
    log::info!("data plane attached fd={}", fd);
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
                    "\nTUN：{} 个 IP 包 / {} 字节 · TCP 连接 {} · DNS 查询 {}",
                    h.packets.load(Ordering::Relaxed),
                    h.bytes.load(Ordering::Relaxed),
                    h.conns.load(Ordering::Relaxed),
                    h.dns.load(Ordering::Relaxed),
                ),
                None => "\nTUN：数据面未启动".to_string(),
            }
        } else {
            "\nTUN：状态锁异常".to_string()
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
    if let Ok(mut g) = engine.client.lock() {
        *g = None;
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
