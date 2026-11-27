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
const UDP_POLL_MS: u64 = 20;
const FLOW_MAP_MAX: usize = 4096;

#[derive(Clone)]
struct TunHandle {
    stop: Arc<AtomicBool>,
    packets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    conns: Arc<AtomicU64>,
    dns: Arc<AtomicU64>,
    udp: Arc<AtomicU64>,       // 从 TUN 收到的 UDP 数据报数
    udp_sent: Arc<AtomicU64>,  // 成功交给 UdpSession 发出的数据报数
    udp_ok: Arc<AtomicBool>,   // 服务端是否通告了 UDP 转发能力
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

// 一个 UDP 流的两端（以 App 发出的方向记录）。
// 构造应答时把 src/dst 互换即可，DNS 与普通 UDP 共用。
#[derive(Clone, Copy)]
struct LocalEp {
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
}

struct UdpReq {
    ep: LocalEp,
    payload: Vec<u8>,
    target: String,      // 交给 UdpSession 的 "host:port"
    dns_id: Option<u16>, // DNS 事务 ID，用于把并发应答精确送回原查询端口
}

// 解析一个完整（未分片）的 IPv4 UDP 包。
// 虚拟 DNS 10.0.0.1:53 改写成上游 8.8.8.8:53，其余按原始目标转发。
fn parse_udp(buf: &[u8]) -> Option<UdpReq> {
    if buf.len() < 28 || buf[0] >> 4 != 4 {
        return None;
    }
    let ihl = ((buf[0] & 0x0f) as usize) * 4;
    if ihl < 20 || buf.len() < ihl + 8 || buf[9] != 17 {
        return None;
    }
    // IPv4 flags/fragment offset：MF(0x2000) 或 offset(0x1fff) 非 0 即为分片。
    // 本步不做重组，分片包直接丢弃（避免构造出错误应答）。
    let frag = u16::from_be_bytes([buf[6], buf[7]]);
    if frag & 0x3fff != 0 {
        return None;
    }
    let mut src_ip = [0u8; 4];
    src_ip.copy_from_slice(&buf[12..16]);
    let mut dst_ip = [0u8; 4];
    dst_ip.copy_from_slice(&buf[16..20]);
    let u = &buf[ihl..];
    let src_port = u16::from_be_bytes([u[0], u[1]]);
    let dst_port = u16::from_be_bytes([u[2], u[3]]);
    let ulen = u16::from_be_bytes([u[4], u[5]]) as usize;
    if ulen < 8 || ihl + ulen > buf.len() {
        return None;
    }
    let payload = buf[ihl + 8..ihl + ulen].to_vec();

    // 多播 / 广播 / 回环 / 本地 TUN 子网（虚拟 DNS 除外）不送去服务端。
    if dst_ip[0] == 0
        || dst_ip[0] == 127
        || dst_ip[0] >= 224
        || (dst_ip[0] == 10 && dst_ip[1] == 0 && dst_ip[2] == 0 && dst_ip[3] != 1)
    {
        return None;
    }

    let is_dns = dst_ip == VIRTUAL_DNS && dst_port == 53;
    let target = if is_dns {
        DNS_UPSTREAM.to_string()
    } else {
        format!(
            "{}.{}.{}.{}:{}",
            dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3], dst_port
        )
    };
    // DNS 头前 2 字节是事务 ID；UDP DNS 报文没有 TCP 那层长度前缀。
    let dns_id = if is_dns && payload.len() >= 12 {
        Some(u16::from_be_bytes([payload[0], payload[1]]))
    } else {
        None
    };
    Some(UdpReq {
        ep: LocalEp {
            src_ip,
            src_port,
            dst_ip,
            dst_port,
        },
        payload,
        target,
        dns_id,
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

// 手工构造 IPv4 + UDP 应答写回 TUN（含 UDP 伪首部校验和，部分 ROM 会校验）。
fn build_udp_reply(ep: LocalEp, payload: &[u8]) -> Vec<u8> {
    let src_ip = ep.dst_ip;
    let src_port = ep.dst_port;
    let dst_ip = ep.src_ip;
    let dst_port = ep.src_port;
    let udp_len = 8 + payload.len();
    let total = 20 + udp_len;
    let mut p = vec![0u8; total];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // Don't Fragment
    p[8] = 64;
    p[9] = 17; // UDP
    p[12..16].copy_from_slice(&src_ip);
    p[16..20].copy_from_slice(&dst_ip);
    p[20..22].copy_from_slice(&src_port.to_be_bytes());
    p[22..24].copy_from_slice(&dst_port.to_be_bytes());
    p[24..26].copy_from_slice(&(udp_len as u16).to_be_bytes());
    p[28..].copy_from_slice(payload);
    let c = ip_checksum(&p[..20]);
    p[10..12].copy_from_slice(&c.to_be_bytes());

    let mut pseudo = Vec::with_capacity(12 + udp_len);
    pseudo.extend_from_slice(&src_ip);
    pseudo.extend_from_slice(&dst_ip);
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

// TUN fd 会被三个线程写（smoltcp TxToken / UDP worker / DNS 降级 worker）。
// 每次 write 对应一个完整 IP 包，用同一把锁保证包边界不被打散。
fn write_tun(fd: i32, lock: &StdMutex<()>, pkt: &[u8]) {
    if let Ok(_guard) = lock.lock() {
        unsafe {
            libc::write(fd, pkt.as_ptr() as *const libc::c_void, pkt.len());
        }
    }
}

// ---------- smoltcp Device：包装 TUN fd ----------

struct Shared {
    cache: VecDeque<Vec<u8>>, // 等监听 socket 建好再投递的 TCP SYN
    new_ports: VecDeque<u16>,
    known_ports: HashSet<u16>,
    udp_reqs: VecDeque<UdpReq>, // 所有 UDP 包在此排队，由 tokio 侧转发
}

struct TunDevice {
    fd: i32,
    shared: Rc<RefCell<Shared>>,
    packets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    write_lock: Arc<StdMutex<()>>,
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
    write_lock: Arc<StdMutex<()>>,
}
impl phy::TxToken for MyTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        write_tun(self.fd, &self.write_lock, &buffer);
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
                return Some((
                    MyRxToken { buffer: pkt },
                    MyTxToken {
                        fd: self.fd,
                        write_lock: self.write_lock.clone(),
                    },
                ));
            }
        }

        let mut buf = vec![0u8; 65536];
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            return None; // O_NONBLOCK 下无数据
        }
        buf.truncate(n as usize);
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);

        // 所有 IPv4 UDP 包都不进 smoltcp：可转发的交给 worker，其余直接丢弃。
        // 不交给协议栈可避免 any_ip 下回 ICMP port unreachable 让 App 误判连接失败。
        if buf.len() >= 20 && buf[0] >> 4 == 4 && buf[9] == 17 {
            if let Some(req) = parse_udp(&buf) {
                self.shared.borrow_mut().udp_reqs.push_back(req);
            }
            return None;
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

        Some((
            MyRxToken { buffer: buf },
            MyTxToken {
                fd: self.fd,
                write_lock: self.write_lock.clone(),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(MyTxToken {
            fd: self.fd,
            write_lock: self.write_lock.clone(),
        })
    }
}

// ---------- 异步侧：TCP relay、UDP 会话、DNS 降级 ----------

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

// 降级路径：服务端未通告 UDP 时，DNS 用 DNS over TCP 并发查询（每个查询独立 task）。
async fn dns_tcp_query(
    client: Arc<rsteria2::ReconnectableClient>,
    req: UdpReq,
    fd: i32,
    lock: Arc<StdMutex<()>>,
) {
    let mut s = match timeout(Duration::from_secs(10), client.tcp_connect(DNS_UPSTREAM)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            log::warn!("dns(tcp) upstream connect failed: {}", e);
            return;
        }
        Err(_) => {
            log::warn!("dns(tcp) upstream connect timeout");
            return;
        }
    };
    let len = req.payload.len() as u16;
    if s.write_all(&len.to_be_bytes()).await.is_err()
        || s.write_all(&req.payload).await.is_err()
    {
        return;
    }
    let mut lb = [0u8; 2];
    match timeout(Duration::from_secs(8), s.read_exact(&mut lb)).await {
        Ok(Ok(_)) => {}
        _ => return,
    }
    let n = u16::from_be_bytes(lb) as usize;
    if n == 0 || n > 4096 {
        return;
    }
    let mut resp = vec![0u8; n];
    match timeout(Duration::from_secs(8), s.read_exact(&mut resp)).await {
        Ok(Ok(_)) => {}
        _ => return,
    }
    write_tun(fd, &lock, &build_udp_reply(req.ep, &resp));
}

async fn dns_tcp_worker(
    client: Arc<rsteria2::ReconnectableClient>,
    fd: i32,
    lock: Arc<StdMutex<()>>,
    mut rx: mpsc::Receiver<UdpReq>,
) {
    while let Some(req) = rx.recv().await {
        let c = client.clone();
        let l = lock.clone();
        tokio::spawn(async move {
            dns_tcp_query(c, req, fd, l).await;
        });
    }
}

// 把降级期间收到的 DNS 请求继续转给 TCP worker（普通 UDP 只能丢弃）。
async fn fallback_loop(
    tun: TunHandle,
    mut rx: mpsc::Receiver<UdpReq>,
    dns_tcp_tx: mpsc::Sender<UdpReq>,
) {
    loop {
        if tun.stop.load(Ordering::Relaxed) {
            break;
        }
        match timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(req)) => {
                if req.dns_id.is_some() && dns_tcp_tx.try_send(req).is_err() {
                    log::warn!("dns fallback queue full, query dropped");
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
}

// 全局 UDP 会话：单个 task 独占 UdpSession（recv 需要 &mut self）。
// 用「20ms 短等应答 + try_recv 批量发包」的轮询，避开 select! 里
// recv(&mut) 与 send(&self) 同时借用 session 的冲突，编译期确定安全。
async fn udp_worker(
    client: Arc<rsteria2::ReconnectableClient>,
    fd: i32,
    lock: Arc<StdMutex<()>>,
    tun: TunHandle,
    mut rx: mpsc::Receiver<UdpReq>,
    dns_tcp_tx: mpsc::Sender<UdpReq>,
) {
    let mut session: Option<rsteria2::UdpSession> = None;
    for attempt in 0..3 {
        match timeout(Duration::from_secs(10), client.udp()).await {
            Ok(Ok(s)) => {
                session = Some(s);
                break;
            }
            Ok(Err(e)) => log::warn!("udp session unavailable (try {}): {}", attempt + 1, e),
            Err(_) => log::warn!("udp session connect timeout (try {})", attempt + 1),
        }
    }
    if session.is_none() {
        tun.udp_ok.store(false, Ordering::Relaxed);
        log::warn!("server has no UDP relay -> DNS falls back to TCP, other UDP dropped");
        fallback_loop(tun.clone(), rx, dns_tcp_tx.clone()).await;
        return;
    }
    tun.udp_ok.store(true, Ordering::Relaxed);
    let mut session = session.unwrap();
    let mut dns_map: HashMap<u16, LocalEp> = HashMap::new();
    let mut flow_map: HashMap<String, LocalEp> = HashMap::new();
    log::info!("udp relay session ready");

    loop {
        if tun.stop.load(Ordering::Relaxed) {
            break;
        }

        // 1) 短等一个远端应答（mpsc::Receiver::recv 可安全取消，超时不丢包）
        let recv_res = timeout(Duration::from_millis(UDP_POLL_MS), session.recv()).await;
        match recv_res {
            Ok(Ok((payload, src))) => {
                // DNS 应答按事务 ID 精确回给原查询端口，避免并发交错回错端口。
                let by_dns = if src == DNS_UPSTREAM && payload.len() >= 12 {
                    let id = u16::from_be_bytes([payload[0], payload[1]]);
                    dns_map.remove(&id)
                } else {
                    None
                };
                let ep = by_dns.or_else(|| flow_map.get(&src).copied());
                match ep {
                    Some(ep) => write_tun(fd, &lock, &build_udp_reply(ep, &payload)),
                    None => log::debug!("udp reply without local flow: {}", src),
                }
            }
            Ok(Err(e)) => {
                // QUIC 重连后旧 session 会失效，重建一个并清空映射表。
                log::warn!("udp session lost: {} -> rebuilding", e);
                let mut rebuilt = None;
                for _ in 0..3 {
                    match timeout(Duration::from_secs(10), client.udp()).await {
                        Ok(Ok(s)) => {
                            rebuilt = Some(s);
                            break;
                        }
                        _ => {}
                    }
                }
                match rebuilt {
                    Some(s) => {
                        session = s;
                        dns_map.clear();
                        flow_map.clear();
                        log::info!("udp session rebuilt");
                    }
                    None => {
                        tun.udp_ok.store(false, Ordering::Relaxed);
                        log::warn!("udp rebuild failed -> DNS falls back to TCP");
                        fallback_loop(tun.clone(), rx, dns_tcp_tx.clone()).await;
                        return;
                    }
                }
            }
            Err(_) => {} // 20ms 无应答，正常
        }

        // 2) 批量排空本地待发的 UDP 包
        while let Ok(req) = rx.try_recv() {
            if dns_map.len() >= FLOW_MAP_MAX {
                dns_map.clear();
            }
            if flow_map.len() >= FLOW_MAP_MAX {
                flow_map.clear();
            }
            if let Some(id) = req.dns_id {
                dns_map.insert(id, req.ep);
            }
            flow_map.insert(req.target.clone(), req.ep);
            match session.send(&req.payload, &req.target) {
                Ok(()) => {
                    tun.udp_sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => log::warn!("udp send {} failed: {}", req.target, e),
            }
        }
    }
    log::info!("udp worker stopped");
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

fn run_stack(fd: i32, client: Arc<rsteria2::ReconnectableClient>, rt: Handle, tun: TunHandle) {
    set_nonblocking(fd);
    log::info!("stack started fd={}", fd);

    let write_lock = Arc::new(StdMutex::new(()));
    let shared = Rc::new(RefCell::new(Shared {
        cache: VecDeque::new(),
        new_ports: VecDeque::new(),
        known_ports: HashSet::new(),
        udp_reqs: VecDeque::new(),
    }));
    let (udp_tx, udp_rx) = mpsc::channel::<UdpReq>(256);
    let (dns_tcp_tx, dns_tcp_rx) = mpsc::channel::<UdpReq>(64);

    {
        let c = client.clone();
        let l = write_lock.clone();
        let t = tun.clone();
        rt.spawn(async move {
            udp_worker(c, fd, l, t, udp_rx, dns_tcp_tx).await;
        });
    }
    {
        let c = client.clone();
        let l = write_lock.clone();
        rt.spawn(async move {
            dns_tcp_worker(c, fd, l, dns_tcp_rx).await;
        });
    }

    let mut device = TunDevice {
        fd,
        shared: shared.clone(),
        packets: tun.packets.clone(),
        bytes: tun.bytes.clone(),
        write_lock: write_lock.clone(),
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
        // 1) 取走 Device 攒下的新端口与 UDP 请求
        let mut need_listen: Vec<u16> = Vec::new();
        {
            let mut sh = shared.borrow_mut();
            while let Some(p) = sh.new_ports.pop_front() {
                sh.known_ports.insert(p);
            }
            while let Some(req) = sh.udp_reqs.pop_front() {
                let is_dns = req.dns_id.is_some();
                tun.udp.fetch_add(1, Ordering::Relaxed);
                if is_dns {
                    tun.dns.fetch_add(1, Ordering::Relaxed);
                }
                if udp_tx.try_send(req).is_err() {
                    log::warn!("udp queue full, datagram dropped");
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
        udp: Arc::new(AtomicU64::new(0)),
        udp_sent: Arc::new(AtomicU64::new(0)),
        udp_ok: Arc::new(AtomicBool::new(false)),
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
                Some(h) => {
                    let udp = h.udp.load(Ordering::Relaxed);
                    let mode = if h.udp_ok.load(Ordering::Relaxed) {
                        "UDP 会话已启用"
                    } else if udp > 0 {
                        "UDP 未启用（服务端不支持，DNS 已降级为 TCP）"
                    } else {
                        "UDP 会话初始化中"
                    };
                    format!(
                        "\nTUN：{} 个 IP 包 / {} 字节 · TCP 连接 {} · DNS 查询 {}\nUDP：收到 {} / 已转发 {} · {}",
                        h.packets.load(Ordering::Relaxed),
                        h.bytes.load(Ordering::Relaxed),
                        h.conns.load(Ordering::Relaxed),
                        h.dns.load(Ordering::Relaxed),
                        udp,
                        h.udp_sent.load(Ordering::Relaxed),
                        mode,
                    )
                }
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
