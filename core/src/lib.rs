use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jboolean, jint, jlong, jstring};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::{Arc, Mutex as StdMutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant as StdInstant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, Mutex};
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::time::timeout;
use serde::Deserialize;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address};

const TUN_ADDR: [u8; 4] = [10, 0, 0, 2];
const VIRTUAL_DNS: [u8; 4] = [10, 0, 0, 1];
const DNS_UPSTREAM: &str = "8.8.8.8:53";
const MTU: usize = 1500;
const SOCK_BUF: usize = 16384;
const LISTEN_BACKLOG: usize = 8;
const MAX_KNOWN_PORTS: usize = 32;
const PORT_IDLE_TTL_SECS: u64 = 120;
// P0-4：不再用 20ms 轮询，也不再靠事务 ID 或远端地址反推应答归属。
const UDP_FLOW_TTL_SECS: u64 = 60;        // 普通 UDP 流空闲回收
const DNS_FLOW_TTL_SECS: u64 = 10;        // DNS 短事务，更快回收
const UDP_FLOW_QUEUE: usize = 64;         // 单流待发队列深度
const MAX_UDP_FLOWS: usize = 256;         // 活跃流上限，超限淘汰最久未见
const UDP_SESSION_TIMEOUT_SECS: u64 = 10; // 建会话超时
const TCP_IDLE_TTL_SECS: u64 = 300;
const GC_INTERVAL_SECS: u64 = 5;          // dispatcher 清理已结束 / 超时的流

// ---------- 批次 4-A：缓冲复用 + 事件驱动唤醒 ----------
const RX_BUF_SIZE: usize = 2048;       // TUN 读缓冲：MTU=1500，2048 足够，且只分配一次
const TX_STACK_SIZE: usize = 2048;     // 出站 TCP 包走栈缓冲，常规路径零堆分配
const UDP_REPLY_CAP: usize = MTU + 64; // 每流一个复用应答缓冲的初始容量
const POLL_MAX_WAIT_MS: u64 = 250;     // poll 超时上限：兼顾 stop 响应与空闲省电

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
    udp_flows: Arc<AtomicU64>, // 当前活跃的 per-flow UDP 会话数
    ipv6: Arc<AtomicU64>,       // 被封堵（丢弃）的 IPv6 包数
    reconnects: Arc<AtomicU64>, // UDP 会话失效 / 重建次数
    wakes: Arc<AtomicU64>,      // 异步侧主动唤醒主循环的次数（验收事件驱动生效）
}

struct Engine {
    rt: Runtime,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    status: Arc<StdMutex<String>>,
    tun: StdMutex<Option<TunHandle>>,
    // 数据面 OS 线程：destroy 前必须先 stop 并 join，避免 Runtime 先于线程销毁
    stack_thread: StdMutex<Option<std::thread::JoinHandle<()>>>,
    client: StdMutex<Option<Arc<rsteria2::ReconnectableClient>>>,
    // 当前生效配置的摘要：nativeGetStatus 会常驻回显，便于确认配置真的传进内核
    config_summary: Arc<StdMutex<String>>,
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

// 任意 IPv4 TCP 包的目标端口，用于刷新 known_ports 的活跃时间。
fn tcp_dst_port(buf: &[u8]) -> Option<u16> {
    if buf.len() < 20 || buf[0] >> 4 != 4 {
        return None;
    }
    let ihl = ((buf[0] & 0x0f) as usize) * 4;
    if ihl < 20 || buf.len() < ihl + 20 || buf[9] != 6 {
        return None;
    }
    let t = &buf[ihl..];
    Some(u16::from_be_bytes([t[2], t[3]]))
}

// 提取「新 SYN」的目标端口：smoltcp 的 listen() 不允许 port=0，
// 必须按目标端口动态创建监听 socket，因此要在包进协议栈前先看到端口。
fn syn_port(buf: &[u8]) -> Option<u16> {
    let port = tcp_dst_port(buf)?;
    let ihl = ((buf[0] & 0x0f) as usize) * 4;
    let flags = buf[ihl + 13];
    if flags & 0x02 != 0 && flags & 0x10 == 0 {
        Some(port)
    } else {
        None
    }
}

// 一个 UDP 流的两端（以 App 发出的方向记录）。
// 构造应答时把 src/dst 互换即可，DNS 与普通 UDP 共用。
// P0-4：四元组直接作为 per-flow session 的 key，同时用于构造应答。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct LocalEp {
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
}

fn is_dns_ep(ep: LocalEp) -> bool {
    ep.dst_ip == VIRTUAL_DNS && ep.dst_port == 53
}

struct UdpReq {
    ep: LocalEp,
    payload: Vec<u8>,
    target: String, // 交给 UdpSession 的 "host:port"
    is_dns: bool,   // 仅用于降级路径：服务端无 UDP 时 DNS 改走 TCP
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
    Some(UdpReq {
        ep: LocalEp {
            src_ip,
            src_port,
            dst_ip,
            dst_port,
        },
        payload,
        target,
        is_dns,
    })
}

// 4-A：把「累加 / 收尾」拆开，UDP 伪首部就能直接在累加器里算完，
// 不再 Vec::with_capacity 拼一段临时缓冲。
fn checksum_add(sum: &mut u32, data: &[u8]) {
    let mut i = 0;
    while i + 1 < data.len() {
        *sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        *sum += (data[i] as u32) << 8;
    }
}

fn checksum_finish(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn ip_checksum(h: &[u8]) -> u16 {
    let mut sum = 0u32;
    checksum_add(&mut sum, h);
    checksum_finish(sum)
}

// 4-A：手工构造 IPv4 + UDP 应答写入「调用方长期持有」的复用缓冲。
// clear() + resize() 复用同一块堆内存，密集 DNS / UDP 应答零分配。
fn build_udp_reply_into(out: &mut Vec<u8>, ep: LocalEp, payload: &[u8]) {
    let src_ip = ep.dst_ip;
    let src_port = ep.dst_port;
    let dst_ip = ep.src_ip;
    let dst_port = ep.src_port;
    let udp_len = 8 + payload.len();
    let total = 20 + udp_len;
    out.clear();
    out.resize(total, 0);
    let p = out.as_mut_slice();
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

    // 伪首部 12 字节 + UDP 报文直接累加，不再分配临时 Vec。
    let mut sum = 0u32;
    checksum_add(&mut sum, &src_ip);
    checksum_add(&mut sum, &dst_ip);
    checksum_add(&mut sum, &[0u8, 17u8]);
    checksum_add(&mut sum, &(udp_len as u16).to_be_bytes());
    checksum_add(&mut sum, &p[20..]);
    let uc = checksum_finish(sum);
    p[26..28].copy_from_slice(&(if uc == 0 { 0xffff } else { uc }).to_be_bytes());
}

// 降级 DNS over TCP 等一次性路径仍用这个包装。
fn build_udp_reply(ep: LocalEp, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(UDP_REPLY_CAP);
    build_udp_reply_into(&mut out, ep, payload);
    out
}

// TUN fd 由 Arc<OwnedFd> 共享所有权：最后一个引用 drop 时才 close，
// 异步 worker 即使比 run_stack 活得久也不会写到被系统复用的 fd 上。
// 单次 write 对应一个完整 IP 包；显式处理 EINTR / EAGAIN 与短写，避免静默丢包。
fn write_tun(fd: &Arc<OwnedFd>, lock: &StdMutex<()>, pkt: &[u8]) {
    let raw = fd.as_raw_fd();
    let _guard = match lock.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut off = 0usize;
    for _ in 0..16 {
        let n = unsafe {
            libc::write(raw, pkt[off..].as_ptr() as *const libc::c_void, pkt.len() - off)
        };
        if n > 0 {
            off += n as usize;
            if off >= pkt.len() {
                return;
            }
            continue;
        }
        if n == 0 {
            break;
        }
        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if err == libc::EINTR || err == libc::EAGAIN || err == libc::EWOULDBLOCK {
            continue;
        }
        log::warn!("tun write failed: {}", std::io::Error::last_os_error());
        return;
    }
    log::warn!("tun write incomplete: {}/{} bytes", off, pkt.len());
}

// 4-A：异步侧唤醒主循环的句柄。write 为 None 表示 pipe 创建失败，
// 主循环会自动退回 20ms 有界轮询，功能不受影响。
#[derive(Clone)]
struct Wake {
    write: Option<Arc<OwnedFd>>,
    count: Arc<AtomicU64>,
}

impl Wake {
    fn fire(&self) {
        if let Some(w) = &self.write {
            let byte = [0u8; 1];
            // 非阻塞写：管道满说明已有待处理事件，忽略 EAGAIN 即可。
            unsafe {
                libc::write(w.as_raw_fd(), byte.as_ptr() as *const libc::c_void, 1);
            }
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ---------- smoltcp Device：包装 TUN fd ----------

struct Shared {
    cache: VecDeque<Vec<u8>>, // 等监听 socket 建好再投递的 TCP SYN
    new_ports: VecDeque<u16>,
    known_ports: HashMap<u16, StdInstant>, // 端口 -> 最近活跃时间，用于空闲回收
    udp_reqs: VecDeque<UdpReq>,           // 所有 UDP 包在此排队，由 tokio 侧转发
}

struct TunDevice {
    fd: Arc<OwnedFd>,
    shared: Rc<RefCell<Shared>>,
    packets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    ipv6: Arc<AtomicU64>,
    write_lock: Arc<StdMutex<()>>,
    rx_buf: Vec<u8>, // 4-A：只分配一次的 TUN 读缓冲
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
    fd: Arc<OwnedFd>,
    write_lock: Arc<StdMutex<()>>,
}
impl phy::TxToken for MyTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // P1-5：常规包（<= MTU）走 2KB 栈缓冲，零堆分配；异常大包才退回堆。
        if len <= TX_STACK_SIZE {
            let mut stack = [0u8; TX_STACK_SIZE];
            let result = f(&mut stack[..len]);
            write_tun(&self.fd, &self.write_lock, &stack[..len]);
            result
        } else {
            let mut buffer = vec![0u8; len];
            let result = f(&mut buffer);
            write_tun(&self.fd, &self.write_lock, &buffer);
            result
        }
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

    // P1-7：smoltcp 0.14 的 Interface::poll() 内部有 ingress loop，
    // receive() 返回 None 会 break 整轮。所以这里自己循环读 TUN：
    // IPv6 / UDP / 新 SYN 都 continue 掉，直到拿到一个该交给协议栈的 TCP 包或 EAGAIN。
    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        loop {
            {
                let mut sh = self.shared.borrow_mut();
                if let Some(pkt) = sh.cache.pop_front() {
                    return Some((
                        MyRxToken { buffer: pkt },
                        MyTxToken {
                            fd: self.fd.clone(),
                            write_lock: self.write_lock.clone(),
                        },
                    ));
                }
            }

            // 4-A：复用 rx_buf，不再每包分配并清零 64KB。MyRxToken 必须拥有
            // 数据，所以 still 需要一次只拷 n 字节的 to_vec()，但分配量从
            // 「每包 64KB」降到「全程一次 2KB + 每包 n 字节」。
            // VpnService MTU 已固定 1500，2048 缓冲不会截断 IP 包。
            let rx_len = self.rx_buf.len();
            let rx_ptr = self.rx_buf.as_mut_ptr();
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    rx_ptr as *mut libc::c_void,
                    rx_len,
                )
            };
            if n <= 0 {
                return None; // O_NONBLOCK 下无数据
            }
            let mut buf = self.rx_buf[..n as usize].to_vec();
            self.packets.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(n as u64, Ordering::Relaxed);

            // Step 4：全局路由已加 ::/0，IPv6 全部进 TUN 后在此直接丢弃，
            // 避免它走物理网卡绕隧道泄漏。不交给 smoltcp（只启用 proto-ipv4），也不回 ICMPv6。
            if buf.len() >= 20 && buf[0] >> 4 == 6 {
                self.ipv6.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // 所有 IPv4 UDP 包都不进 smoltcp：可转发的交给 worker，其余直接丢弃。
            // 不交给协议栈可避免 any_ip 下回 ICMP port unreachable 让 App 误判连接失败。
            if buf.len() >= 20 && buf[0] >> 4 == 4 && buf[9] == 17 {
                if let Some(req) = parse_udp(&buf) {
                    self.shared.borrow_mut().udp_reqs.push_back(req);
                }
                continue;
            }

            let mut sh = self.shared.borrow_mut();
            if let Some(port) = tcp_dst_port(&buf) {
                if let Some(t) = sh.known_ports.get_mut(&port) {
                    *t = StdInstant::now();
                }
            }
            if let Some(port) = syn_port(&buf) {
                if !sh.known_ports.contains_key(&port) {
                    // 缓存该 SYN，本轮继续读后面的包；外层循环先建好 backlog，
                    // 下一轮 poll 的 ingress loop 会连续投递，避免同端口并发 SYN 被 RST。
                    sh.new_ports.push_back(port);
                    sh.cache.push_back(buf);
                    continue;
                }
            }
            drop(sh);

            return Some((
                MyRxToken { buffer: buf },
                MyTxToken {
                    fd: self.fd.clone(),
                    write_lock: self.write_lock.clone(),
                },
            ));
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(MyTxToken {
            fd: self.fd.clone(),
            write_lock: self.write_lock.clone(),
        })
    }
}

// ---------- 异步侧：TCP relay、UDP 会话、DNS 降级 ----------

struct Conn {
    to_remote: mpsc::Sender<Vec<u8>>,
    from_remote: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
    last_active: StdInstant,
    connected: Arc<AtomicBool>, // tcp_connect 是否成功，决定远端结束时发 FIN 还是 RST
    remote_closed: bool,       // from_remote 已 Disconnected
    app_fin: bool,             // 已向 relay 发送过 EOF 标记（空 Vec）
    close_done: bool,          // 已请求 smoltcp 关闭，等状态机走完再 remove
}

async fn tcp_relay(
    stream: rsteria2::DuplexStream,
    mut to_rx: mpsc::Receiver<Vec<u8>>,
    from_tx: mpsc::Sender<Vec<u8>>,
    connected: Arc<AtomicBool>,
    wake: Wake,
) {
    connected.store(true, Ordering::Relaxed);
    let (mut r, mut w) = tokio::io::split(stream);
    let mut buf = vec![0u8; 16384];
    let mut write_closed = false;
    loop {
        tokio::select! {
            item = to_rx.recv(), if !write_closed => match item {
                // 空 Vec 是主循环发来的「App 侧 FIN」标记：半关闭写方向，
                // 然后继续读远端剩余数据直到 EOF，让 from_tx 自然 drop。
                Some(data) if data.is_empty() => {
                    let _ = w.shutdown().await;
                    write_closed = true;
                }
                Some(data) => {
                    if w.write_all(&data).await.is_err() { break; }
                }
                None => {
                    let _ = w.shutdown().await;
                    write_closed = true;
                    break;
                }
            },
            res = r.read(&mut buf) => match res {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if from_tx.send(buf[..n].to_vec()).await.is_err() { break; }
                    // 4-A：远端数据已进 channel，立刻唤醒主循环搬运到 smoltcp socket。
                    wake.fire();
                }
            },
        }
    }
    if !write_closed {
        let _ = w.shutdown().await;
    }
    // 无论正常结束还是异常断开都唤醒：让主循环立刻看到 from_remote 的
    // Disconnected 并发出 FIN / RST，不用等下一次定时轮询。
    wake.fire();
}

// 降级路径：服务端未通告 UDP 时，DNS 用 DNS over TCP 并发查询（每个查询独立 task）。
async fn dns_tcp_query(
    client: Arc<rsteria2::ReconnectableClient>,
    req: UdpReq,
    fd: Arc<OwnedFd>,
    lock: Arc<StdMutex<()>>,
    stop: Arc<AtomicBool>,
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
    // 最长约 26 秒的窗口：写回前再确认数据面没停，避免往已断开的隧道回包。
    if stop.load(Ordering::Relaxed) {
        return;
    }
    write_tun(&fd, &lock, &build_udp_reply(req.ep, &resp));
}

async fn dns_tcp_worker(
    client: Arc<rsteria2::ReconnectableClient>,
    fd: Arc<OwnedFd>,
    lock: Arc<StdMutex<()>>,
    stop: Arc<AtomicBool>,
    mut rx: mpsc::Receiver<UdpReq>,
) {
    while let Some(req) = rx.recv().await {
        let c = client.clone();
        let f = fd.clone();
        let l = lock.clone();
        let s = stop.clone();
        tokio::spawn(async move {
            dns_tcp_query(c, req, f, l, s).await;
        });
    }
}

// P0-4：按本地四元组每流一个 UdpSession。UdpSessionManager 按 session_id
// 把应答分发到各会话独立 channel，单 session 处理所有流必然串包。
// 每个流一个 task，用 select! 同时等远端应答、本地待发包、空闲超时。
// session.send() 刻意放在 select! 之外：避免 recv(&mut self) 与 send(&self)
// 在同一 select! 里的借用争议。recv() 基于 mpsc::Receiver::recv().await，
// 取消安全，被取消的应答仍留在 channel 里，下一轮一定还会取到。

enum FlowEvent {
    Reply(Vec<u8>),
    Out(UdpReq),
    Failed,
    Stopped,
}

async fn flow_task(
    client: Arc<rsteria2::ReconnectableClient>,
    fd: Arc<OwnedFd>,
    lock: Arc<StdMutex<()>>,
    tun: TunHandle,
    ep: LocalEp,
    idle_secs: u64,
    mut rx: mpsc::Receiver<UdpReq>,
) {
    let mut session = match timeout(
        Duration::from_secs(UDP_SESSION_TIMEOUT_SECS),
        client.udp(),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            log::warn!("udp session for local port {} unavailable: {}", ep.src_port, e);
            tun.reconnects.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            log::warn!("udp session connect timeout");
            tun.reconnects.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let idle_dur = Duration::from_secs(idle_secs);
    let mut idle = tokio::time::sleep(idle_dur);
    tokio::pin!(idle);
    // 本流复用同一个应答缓冲：DNS / UDP 密集回包时不再每个包分配 Vec。
    let mut reply: Vec<u8> = Vec::with_capacity(UDP_REPLY_CAP);

    loop {
        if tun.stop.load(Ordering::Relaxed) {
            break;
        }
        let ev = tokio::select! {
            res = session.recv() => match res {
                Ok((payload, _src)) => FlowEvent::Reply(payload),
                Err(_) => FlowEvent::Failed,
            },
            item = rx.recv() => match item {
                Some(req) => FlowEvent::Out(req),
                None => FlowEvent::Stopped,
            },
            _ = &mut idle => FlowEvent::Stopped,
        };
        match ev {
            // 按流隔离后不再需要事务 ID 映射或远端地址流表：
            // 这个 session 只服务本四元组，应答必然回给 ep 记录的本地端点。
            FlowEvent::Reply(payload) => {
                if !tun.stop.load(Ordering::Relaxed) {
                    build_udp_reply_into(&mut reply, ep, &payload);
                    write_tun(&fd, &lock, &reply);
                }
            }
            FlowEvent::Out(req) => match session.send(&req.payload, &req.target) {
                Ok(()) => {
                    tun.udp_sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => log::warn!("udp send {} failed: {}", req.target, e),
            },
            FlowEvent::Failed => {
                // QUIC 重连后旧 session 会失效；结束本流，dispatcher 下一包重建。
                tun.reconnects.fetch_add(1, Ordering::Relaxed);
                break;
            }
            FlowEvent::Stopped => break,
        }
        idle.as_mut().reset(tokio::time::Instant::now() + idle_dur);
    }
}

struct Flow {
    tx: mpsc::Sender<UdpReq>,
    last_active: StdInstant,
    task: tokio::task::JoinHandle<()>,
}

// 单一 dispatcher 收所有 UDP 包并按四元组分流；不再转移 rx 所有权。
// 服务端无 UDP 时用标志位降级：DNS 转 TCP worker，普通 UDP 丢弃。
async fn udp_dispatcher(
    client: Arc<rsteria2::ReconnectableClient>,
    fd: Arc<OwnedFd>,
    lock: Arc<StdMutex<()>>,
    tun: TunHandle,
    mut rx: mpsc::Receiver<UdpReq>,
    dns_tcp_tx: mpsc::Sender<UdpReq>,
) {
    // 探测：建一个 session 后立即 drop（UdpSession::Drop 会自动注销 id）。
    let mut udp_available = false;
    for attempt in 0..3 {
        match timeout(
            Duration::from_secs(UDP_SESSION_TIMEOUT_SECS),
            client.udp(),
        )
        .await
        {
            Ok(Ok(_probe)) => {
                udp_available = true;
                break;
            }
            Ok(Err(e)) => log::warn!("udp relay unavailable (try {}): {}", attempt + 1, e),
            Err(_) => log::warn!("udp probe timeout (try {})", attempt + 1),
        }
    }
    tun.udp_ok.store(udp_available, Ordering::Relaxed);
    if udp_available {
        log::info!("udp relay ready, per-flow sessions enabled");
    } else {
        log::warn!("server has no UDP relay -> DNS falls back to TCP, other UDP dropped");
    }

    let mut flows: HashMap<LocalEp, Flow> = HashMap::new();
    let gc_dur = Duration::from_secs(GC_INTERVAL_SECS);
    let mut gc_tick = tokio::time::interval(gc_dur);

    loop {
        tun.udp_flows.store(flows.len() as u64, Ordering::Relaxed);
        if tun.stop.load(Ordering::Relaxed) {
            break;
        }
        let item = tokio::select! {
            r = rx.recv() => r,
            _ = gc_tick.tick() => {
                let now = StdInstant::now();
                let ttl = Duration::from_secs(UDP_FLOW_TTL_SECS);
                let stale: Vec<LocalEp> = flows
                    .iter()
                    .filter(|(_, f)| f.task.is_finished() || now.duration_since(f.last_active) > ttl)
                    .map(|(k, _)| *k)
                    .collect();
                for k in stale {
                    if let Some(f) = flows.remove(&k) {
                        f.task.abort();
                    }
                }
                continue;
            }
        };
        let req = match item {
            Some(r) => r,
            None => break,
        };

        if !udp_available {
            if req.is_dns && dns_tcp_tx.try_send(req).is_err() {
                log::warn!("dns fallback queue full, query dropped");
            }
            continue;
        }

        let key = req.ep;
        if !flows.contains_key(&key) {
            if flows.len() >= MAX_UDP_FLOWS {
                let oldest = flows
                    .iter()
                    .min_by(|a, b| a.1.last_active.cmp(&b.1.last_active))
                    .map(|(k, _)| *k);
                if let Some(k) = oldest {
                    if let Some(f) = flows.remove(&k) {
                        f.task.abort();
                    }
                }
            }
            let (tx, flow_rx) = mpsc::channel::<UdpReq>(UDP_FLOW_QUEUE);
            let idle_secs = if is_dns_ep(key) {
                DNS_FLOW_TTL_SECS
            } else {
                UDP_FLOW_TTL_SECS
            };
            let task = tokio::spawn(flow_task(
                client.clone(),
                fd.clone(),
                lock.clone(),
                tun.clone(),
                key,
                idle_secs,
                flow_rx,
            ));
            flows.insert(
                key,
                Flow {
                    tx,
                    last_active: StdInstant::now(),
                    task,
                },
            );
        }

        let mut gone = false;
        if let Some(f) = flows.get_mut(&key) {
            f.last_active = StdInstant::now();
            match f.tx.try_send(req) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    log::warn!("udp flow queue full, datagram dropped");
                }
                Err(_) => gone = true, // Closed 或未来新增变体：会话已结束，下一包重建
            }
        }
        if gone {
            // flow_task 已结束（session 失效 / 空闲），移除后下一包自动重建
            if let Some(f) = flows.remove(&key) {
                f.task.abort();
            }
        }
    }
    for (_, f) in flows.drain() {
        f.task.abort();
    }
    tun.udp_flows.store(0, Ordering::Relaxed);
    log::info!("udp dispatcher stopped");
}

// ---------- 同步侧：smoltcp 主循环（独占一个 OS 线程） ----------

fn set_nonblocking(fd: &Arc<OwnedFd>) {
    let raw = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn run_stack(fd: Arc<OwnedFd>, client: Arc<rsteria2::ReconnectableClient>, rt: Handle, tun: TunHandle) {
    set_nonblocking(&fd);
    let raw = fd.as_raw_fd();
    log::info!("stack started fd={}", raw);

    // 4-A：创建非阻塞 pipe 作为「异步侧 -> 主循环」的唤醒通道。
    // pipe() 是 POSIX 标准符号，不依赖 eventfd，Android / Linux 均可用。
    // 创建失败时 wake_* 全为 None，主循环自动退回 20ms 有界轮询。
    let mut wake_fds = [0i32; 2];
    let wake_ok = unsafe { libc::pipe(wake_fds.as_mut_ptr()) } == 0;
    if wake_ok {
        unsafe {
            for f in wake_fds.iter() {
                let flags = libc::fcntl(*f, libc::F_GETFL);
                if flags >= 0 {
                    libc::fcntl(*f, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }
    } else {
        log::warn!("wake pipe creation failed, fallback to 20ms bounded polling");
    }
    // OwnedFd 接管：run_stack 退出时自动 close，不泄漏 fd。
    let wake_read = if wake_ok {
        Some(unsafe { OwnedFd::from_raw_fd(wake_fds[0]) })
    } else {
        None
    };
    let wake_write = if wake_ok {
        Some(Arc::new(unsafe { OwnedFd::from_raw_fd(wake_fds[1]) }))
    } else {
        None
    };

    let write_lock = Arc::new(StdMutex::new(()));
    let shared = Rc::new(RefCell::new(Shared {
        cache: VecDeque::new(),
        new_ports: VecDeque::new(),
        known_ports: HashMap::new(),
        udp_reqs: VecDeque::new(),
    }));
    let (udp_tx, udp_rx) = mpsc::channel::<UdpReq>(256);
    let (dns_tcp_tx, dns_tcp_rx) = mpsc::channel::<UdpReq>(64);

    {
        let c = client.clone();
        let f = fd.clone();
        let l = write_lock.clone();
        let t = tun.clone();
        rt.spawn(async move {
            udp_dispatcher(c, f, l, t, udp_rx, dns_tcp_tx).await;
        });
    }
    {
        let c = client.clone();
        let f = fd.clone();
        let l = write_lock.clone();
        let s = tun.stop.clone();
        rt.spawn(async move {
            dns_tcp_worker(c, f, l, s, dns_tcp_rx).await;
        });
    }

    let mut device = TunDevice {
        fd: fd.clone(),
        shared: shared.clone(),
        packets: tun.packets.clone(),
        bytes: tun.bytes.clone(),
        ipv6: tun.ipv6.clone(),
        write_lock: write_lock.clone(),
        rx_buf: vec![0u8; RX_BUF_SIZE],
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
    // pipe 创建失败时退回原来的 20ms 有界轮询，避免远端数据被延迟 250ms。
    let poll_cap_ms: u64 = if wake_read.is_some() { POLL_MAX_WAIT_MS } else { 20 };
    let wake = Wake {
        write: wake_write,
        count: tun.wakes.clone(),
    };

    while !tun.stop.load(Ordering::Relaxed) {
        // 1) 取走 Device 攒下的新端口与 UDP 请求
        {
            let mut sh = shared.borrow_mut();
            while let Some(p) = sh.new_ports.pop_front() {
                sh.known_ports.insert(p, StdInstant::now());
            }
            // 端口表上限：超限时淘汰最久未见的，防止 backlog 叠出无界内存
            while sh.known_ports.len() > MAX_KNOWN_PORTS {
                let oldest = sh
                    .known_ports
                    .iter()
                    .min_by(|a, b| a.1.cmp(b.1))
                    .map(|(p, _)| *p);
                match oldest {
                    Some(p) => {
                        sh.known_ports.remove(&p);
                    }
                    None => break,
                }
            }
            while let Some(req) = sh.udp_reqs.pop_front() {
                let is_dns = req.is_dns;
                tun.udp.fetch_add(1, Ordering::Relaxed);
                if is_dns {
                    tun.dns.fetch_add(1, Ordering::Relaxed);
                }
                if udp_tx.try_send(req).is_err() {
                    log::warn!("udp queue full, datagram dropped");
                }
            }
        }

        // 2) 统计现有 socket：每端口 Listen 数量、Listen handle、活跃连接端口
        let handles: Vec<SocketHandle> = sockets.iter().map(|(h, _)| h).collect();
        let mut listen_count: HashMap<u16, usize> = HashMap::new();
        let mut listen_handles: HashMap<u16, Vec<SocketHandle>> = HashMap::new();
        let mut active_ports: HashSet<u16> = HashSet::new();
        for h in &handles {
            let s = sockets.get::<tcp::Socket>(*h);
            if s.state() == tcp::State::Listen {
                let p = s.listen_endpoint().port;
                *listen_count.entry(p).or_insert(0) += 1;
                listen_handles.entry(p).or_default().push(*h);
            } else if let Some(ep) = s.local_endpoint() {
                active_ports.insert(ep.port);
            }
        }

        // 2a) 空闲端口回收：超过 TTL 且无活跃连接，连同它的 Listen socket 一起移除。
        //     这是 P0-2 backlog 的必要配套，否则每端口 8 个 socket 会让内存只增不减。
        let mut retired: Vec<u16> = Vec::new();
        {
            let mut sh = shared.borrow_mut();
            let now = StdInstant::now();
            sh.known_ports.retain(|p, t| {
                let keep = now.duration_since(*t) <= Duration::from_secs(PORT_IDLE_TTL_SECS)
                    || active_ports.contains(p);
                if !keep {
                    retired.push(*p);
                }
                keep
            });
        }
        for p in retired {
            if let Some(hs) = listen_handles.remove(&p) {
                for h in hs {
                    conns.remove(&h);
                    sockets.remove(h);
                }
            }
            listen_count.remove(&p);
        }

        // 2b) 每个已见端口维持 LISTEN_BACKLOG 个 Listen socket。
        //     smoltcp 的 listen socket 没有 backlog 概念，一轮 poll() 的 ingress loop
        //     会连续吃多个同端口 SYN，只有靠「预留多个」才不会被 RST。
        let mut need_listen: Vec<u16> = Vec::new();
        {
            let sh = shared.borrow();
            for p in sh.known_ports.keys() {
                let have = listen_count.get(p).copied().unwrap_or(0);
                for _ in have..LISTEN_BACKLOG {
                    need_listen.push(*p);
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
            let connected = Arc::new(AtomicBool::new(false));
            conns.insert(
                h,
                Conn {
                    to_remote: to_tx,
                    from_remote: from_rx,
                    pending: Vec::new(),
                    last_active: StdInstant::now(),
                    connected: connected.clone(),
                    remote_closed: false,
                    app_fin: false,
                    close_done: false,
                },
            );
            let c = client.clone();
            let t = target.clone();
            let ok = connected.clone();
            let w = wake.clone();
            rt.spawn(async move {
                let r = timeout(Duration::from_secs(15), c.tcp_connect(&t)).await;
                match r {
                    Ok(Ok(s)) => tcp_relay(s, to_rx, from_tx, ok, w.clone()).await,
                    Ok(Err(e)) => {
                        log::error!("tcp_connect {} failed: {}", t, e);
                        drop(from_tx);
                    }
                    Err(_) => {
                        log::error!("tcp_connect {} timeout", t);
                        drop(from_tx);
                    }
                }
                // connect 失败 / relay 结束都唤醒：主循环据此发 RST / FIN。
                w.fire();
            });
            log::info!("new tcp conn -> {}", target);
        }

        // 5) 双向搬运 socket <-> hy2 stream，并正确传播 FIN / RST
        for (h, conn) in conns.iter_mut() {
            if conn.close_done {
                // 已请求关闭：等 smoltcp 状态机把 FIN 发出去、进入 Closed/TimeWait，
                // 由步骤 4 统一回收，绝不当轮 remove（否则 FIN 根本发不出去）。
                continue;
            }
            let mut touched = false;
            let s = sockets.get_mut::<tcp::Socket>(*h);

            if conn.last_active.elapsed() > Duration::from_secs(TCP_IDLE_TTL_SECS) {
                s.close();
                conn.close_done = true;
                continue;
            }

            // App 侧 FIN：收到 FIN 后 may_recv()=false 且无待读数据。
            // 发一个空 Vec 作为 EOF 标记，让 relay 对远端执行 shutdown()。
            if !conn.app_fin && !s.may_recv() && !s.can_recv() {
                if conn.to_remote.try_send(Vec::new()).is_ok() {
                    conn.app_fin = true;
                    touched = true;
                }
            }

            // App -> 远端
            if !conn.remote_closed {
                while s.can_recv() {
                    match conn.to_remote.try_reserve() {
                        Ok(permit) => match s.recv_slice(&mut buf) {
                            Ok(n) if n > 0 => {
                                permit.send(buf[..n].to_vec());
                                touched = true;
                            }
                            _ => break,
                        },
                        Err(_) => break,
                    }
                }
            }

            // 远端 -> App：先把上一段发完
            if !conn.pending.is_empty() {
                match s.send_slice(&conn.pending) {
                    Ok(n) => {
                        conn.pending.drain(..n);
                        touched = true;
                    }
                    Err(_) => {}
                }
            }

            // pending 清空后才判断 relay 是否结束：
            // Disconnected + 连接成功过 -> close() 给 App 发 FIN；
            // Disconnected + 从未连上   -> abort() 给 App 发 RST（立刻报错，不再等 300s）。
            if conn.pending.is_empty() && !conn.remote_closed {
                match conn.from_remote.try_recv() {
                    Ok(d) => {
                        conn.pending = d;
                        touched = true;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        conn.remote_closed = true;
                        conn.close_done = true;
                        if conn.connected.load(Ordering::Relaxed) {
                            s.close();
                        } else {
                            s.abort();
                        }
                        touched = true;
                    }
                }
            }

            if touched {
                conn.last_active = StdInstant::now();
            }
        }

        for h in to_remove.drain(..) {
            conns.remove(&h);
            sockets.remove(h);
        }
        tun.conns.store(conns.len() as u64, Ordering::Relaxed);

        // 6) 事件驱动等待：同时等 TUN 可读、异步侧唤醒、smoltcp 下一次定时器。
        //    poll_delay() 给出协议栈自己需要被 poll 的最迟时间（重传 / keep-alive /
        //    超时），拿它当 poll 超时，定时器就不会因为「睡太久」而卡住；
        //    上限 poll_cap_ms 兼顾 stop 响应与空闲省电。
        //    注意 smoltcp 的 Duration::millis() 是取模值，必须用 total_millis()。
        let wait_ms = match iface.poll_delay(Instant::now(), &sockets) {
            Some(d) => d.total_millis().min(poll_cap_ms).max(1) as i32,
            None => poll_cap_ms as i32,
        };
        unsafe {
            let wake_raw = wake_read.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1);
            let mut pfds = [
                libc::pollfd {
                    fd: raw,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake_raw,
                    events: if wake_raw >= 0 { libc::POLLIN } else { 0 },
                    revents: 0,
                },
            ];
            let nfds: libc::nfds_t = if wake_raw >= 0 { 2 } else { 1 };
            libc::poll(pfds.as_mut_ptr(), nfds, wait_ms);
            if wake_raw >= 0 && (pfds[1].revents & libc::POLLIN) != 0 {
                // 排空非阻塞 pipe：只需知道「有事件」，字节内容无意义。
                let mut drain = [0u8; 64];
                loop {
                    let n = libc::read(
                        wake_raw,
                        drain.as_mut_ptr() as *mut libc::c_void,
                        drain.len(),
                    );
                    if n <= 0 {
                        break;
                    }
                }
            }
        }
    }

    // 不再 libc::close(fd)：Arc<OwnedFd> 的最后一个引用 drop 时才关闭，
    // 异步 worker 即使还持有 clone 也不会写到被系统复用的 fd 上。
    drop(fd);
    log::info!("stack stopped fd={}", raw);
}

// ---------- 配置项暴露 ----------
// rsteria2::Config 只 #[derive(Debug, Clone)]、没有 Deserialize，
// 所以不能直接 serde_json::from_str::<rsteria2::Config>()。
// 这里定义一份「界面镜像结构体」，字段名与 ConfigStore.kt 的 JSONObject key 一一对应，
// 解析 + 校验通过后再映射成真正的 rsteria2::Config。
#[derive(Deserialize)]
struct UiConfig {
    #[serde(default)] server: String,
    #[serde(default)] auth: String,
    #[serde(default)] server_name: String,
    #[serde(default)] insecure: bool,
    /// 上传带宽，单位 Mbps；0 = 走 BBR。映射时换算成 bytes/sec。
    #[serde(default)] tx_mbps: u64,
    /// 下载带宽，单位 Mbps；0 = 让服务端自行测速。
    #[serde(default)] rx_mbps: u64,
    #[serde(default)] obfs_password: String,
    #[serde(default)] hop_ports: String,
    #[serde(default)] hop_interval_min_secs: u64,
    #[serde(default)] hop_interval_max_secs: u64,
    #[serde(default = "default_true")] fast_open: bool,
    #[serde(default)] pin_sha256: String,
}

fn default_true() -> bool {
    true
}

impl UiConfig {
    // panic = "abort"，这里只做返回 Result 的校验，绝不 unwrap / expect。
    fn validate(&self) -> Result<(), String> {
        if self.server.trim().is_empty() {
            return Err("服务器地址不能为空".to_string());
        }
        if self.auth.is_empty() {
            return Err("认证密码不能为空".to_string());
        }
        let obfs = self.obfs_password.trim();
        if !obfs.is_empty() && obfs.len() < 4 {
            return Err("混淆密码至少 4 个字节（Salamander 要求）".to_string());
        }
        let hop = self.hop_ports.trim();
        if !hop.is_empty() && hop != "all" && hop != "*" {
            let ok = hop
                .chars()
                .all(|c| c.is_ascii_digit() || c == ',' || c == '-' || c.is_ascii_whitespace());
            if !ok {
                return Err(
                    "端口跳跃格式不合法；示例：20000-50000 / 443,8443,9000-9100 / all".to_string(),
                );
            }
        }
        let pin = self.pin_sha256.trim();
        if !pin.is_empty() {
            let hex: String = pin.chars().filter(|c| *c != ':').collect();
            if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err("证书 SHA-256 Pin 必须是 64 位十六进制（可带冒号分隔）".to_string());
            }
        }
        Ok(())
    }

    // 1 Mbps = 1_000_000 bit/s = 125_000 byte/s；用饱和乘法防 u64 溢出。
    fn to_rsteria(&self) -> rsteria2::Config {
        rsteria2::Config {
            server_addr: self.server.trim().to_string(),
            server_name: self.server_name.trim().to_string(),
            auth: self.auth.clone(),
            insecure: self.insecure,
            rx_bps: self.rx_mbps.saturating_mul(125_000),
            tx_bps: self.tx_mbps.saturating_mul(125_000),
            obfs_password: self.obfs_password.trim().to_string(),
            hop_ports: self.hop_ports.trim().to_string(),
            hop_interval_min_secs: self.hop_interval_min_secs,
            hop_interval_max_secs: self.hop_interval_max_secs,
            fast_open: self.fast_open,
            pin_sha256: self.pin_sha256.trim().to_string(),
            // ca_pem（PEM 文本过长，界面不暴露）与 quic 调参保持库默认值
            ..Default::default()
        }
    }

    // 状态卡常驻显示，用于确认 insecure / 带宽 / obfs 等确实传到了内核。
    fn summary(&self) -> String {
        format!(
            "SNI={} · insecure={} · tx={}Mbps/rx={}Mbps · obfs={} · hop={} · fastOpen={} · pin={}",
            if self.server_name.trim().is_empty() {
                "自动".to_string()
            } else {
                self.server_name.trim().to_string()
            },
            if self.insecure { "开" } else { "关" },
            self.tx_mbps,
            self.rx_mbps,
            if self.obfs_password.trim().is_empty() { "关" } else { "开" },
            if self.hop_ports.trim().is_empty() {
                "关".to_string()
            } else {
                self.hop_ports.trim().to_string()
            },
            if self.fast_open { "开" } else { "关" },
            if self.pin_sha256.trim().is_empty() { "关" } else { "开" },
        )
    }
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
        stack_thread: StdMutex::new(None),
        client: StdMutex::new(None),
        config_summary: Arc::new(StdMutex::new(String::new())),
    });
    Box::into_raw(engine) as jlong
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    config_json: jni::objects::JString,
) -> jboolean {
    let json: String = match env.get_string(&config_json) {
        Ok(s) => s.into(),
        Err(_) => return false as jboolean,
    };
    if ptr == 0 {
        return false as jboolean;
    }
    let engine = unsafe { &*(ptr as *mut Engine) };

    // rsteria2::Config 无 Deserialize：先解析镜像结构体，再校验，再映射。
    // 任何失败都写进 status 并返回 false，绝不 panic（panic = abort 会杀进程）。
    let ui: UiConfig = match serde_json::from_str(&json) {
        Ok(c) => c,
        Err(e) => {
            set_status(&engine.status, format!("配置解析失败：{}", e));
            return false as jboolean;
        }
    };
    if let Err(e) = ui.validate() {
        set_status(&engine.status, format!("配置无效：{}", e));
        return false as jboolean;
    }

    let summary = ui.summary();
    let cfg = ui.to_rsteria();

    // 数据面与握手验证共用同一个可重连客户端，连接只建一次
    let rc = Arc::new(rsteria2::ReconnectableClient::new(cfg));
    if let Ok(mut g) = engine.client.lock() {
        *g = Some(rc.clone());
    }
    if let Ok(mut g) = engine.config_summary.lock() {
        *g = summary.clone();
    }

    let status = engine.status.clone();
    set_status(&status, format!("正在握手…\n配置：{}", summary));
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

    // 补充发现 5：先停旧数据面线程并 join，再挂新 fd。
    // 否则旧线程仍在 close() 旧 fd 时，系统可能把同一个 fd 号分配给新 TUN。
    if let Ok(mut jg) = engine.stack_thread.lock() {
        if let Some(j) = jg.take() {
            if let Ok(g) = engine.tun.lock() {
                if let Some(h) = g.as_ref() {
                    h.stop.store(true, Ordering::Relaxed);
                }
            }
            let _ = j.join();
        }
    }

    let handle = TunHandle {
        stop: Arc::new(AtomicBool::new(false)),
        packets: Arc::new(AtomicU64::new(0)),
        bytes: Arc::new(AtomicU64::new(0)),
        conns: Arc::new(AtomicU64::new(0)),
        dns: Arc::new(AtomicU64::new(0)),
        udp: Arc::new(AtomicU64::new(0)),
        udp_sent: Arc::new(AtomicU64::new(0)),
        udp_ok: Arc::new(AtomicBool::new(false)),
        udp_flows: Arc::new(AtomicU64::new(0)),
        ipv6: Arc::new(AtomicU64::new(0)),
        reconnects: Arc::new(AtomicU64::new(0)),
        wakes: Arc::new(AtomicU64::new(0)),
    };
    let thread_handle = handle.clone();
    let rt_handle = engine.rt.handle().clone();

    // Kotlin 已 detachFd()，裸 fd 的所有权在这里接管
    let owned = Arc::new(unsafe { OwnedFd::from_raw_fd(fd as i32) });
    let stack_fd = owned.clone();
    let jh = std::thread::spawn(move || run_stack(stack_fd, client, rt_handle, thread_handle));

    if let Ok(mut guard) = engine.tun.lock() {
        *guard = Some(handle);
    }
    if let Ok(mut jg) = engine.stack_thread.lock() {
        *jg = Some(jh);
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
                        "\nTUN：{} 个 IP 包 / {} 字节 · TCP 连接 {} · DNS 查询 {}\nUDP：收到 {} / 已转发 {} · {} · 活跃流 {}\nIPv6 封堵 {} 个包 · UDP 流重建 {} 次 · 主循环唤醒 {} 次",
                        h.packets.load(Ordering::Relaxed),
                        h.bytes.load(Ordering::Relaxed),
                        h.conns.load(Ordering::Relaxed),
                        h.dns.load(Ordering::Relaxed),
                        udp,
                        h.udp_sent.load(Ordering::Relaxed),
                        mode,
                        h.udp_flows.load(Ordering::Relaxed),
                        h.ipv6.load(Ordering::Relaxed),
                        h.reconnects.load(Ordering::Relaxed),
                        h.wakes.load(Ordering::Relaxed),
                    )
                }
                None => "\nTUN：数据面未启动".to_string(),
            }
        } else {
            "\nTUN：状态锁异常".to_string()
        };
        let cfg_line = engine
            .config_summary
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let cfg_info = if cfg_line.is_empty() {
            String::new()
        } else {
            format!("\n配置：{}", cfg_line)
        };
        format!("{}{}{}", base, cfg_info, tun_info)
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
    // 补充发现 6：nativeStop 不能只依赖 Kotlin 先调 nativeDetachTun，
    // 顺序一改就会泄漏数据面线程与 fd。这里也置 stop。
    if let Ok(g) = engine.tun.lock() {
        if let Some(h) = g.as_ref() {
            h.stop.store(true, Ordering::Relaxed);
        }
    }
    set_status(&engine.status, "已断开");
}

#[no_mangle]
pub extern "C" fn Java_com_pcxx_hy2box_Hy2Bridge_nativeDestroy(_env: JNIEnv, _class: JClass, ptr: jlong) {
    if ptr == 0 {
        return;
    }
    // 补充发现 7：先停数据面并 join 线程，再 drop Engine（内含 Runtime）。
    // 用块把 &mut Engine 引用的作用域收住，避免与 Box::from_raw 的重建别名重叠。
    {
        let engine = unsafe { &*(ptr as *mut Engine) };
        if let Ok(g) = engine.tun.lock() {
            if let Some(h) = g.as_ref() {
                h.stop.store(true, Ordering::Relaxed);
            }
        }
        if let Ok(mut jg) = engine.stack_thread.lock() {
            if let Some(j) = jg.take() {
                let _ = j.join();
            }
        }
    }
    unsafe {
        drop(Box::from_raw(ptr as *mut Engine));
    }
}
