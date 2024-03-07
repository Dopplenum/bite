use crate::memory::PAGE_SIZE;
use crate::{Error, ReadMemory, Tracee};

use nix::libc;
use nix::sys::socket::{self, SockaddrLike};
use nix::sys::{signal, stat};

use std::ffi::{c_int, c_long, CString};
use std::fmt;
use std::mem::{size_of, MaybeUninit};
use std::os::fd::AsRawFd;

const ETH_ALL: c_int = libc::ETH_P_ALL.to_be();

macro_rules! print_delimited {
    [$str:expr, $x:expr, $($xs:expr),+] => {{
        $str += &$x;
        $str += ", ";
        print_delimited![$str, $($xs),+];
    }};

    [$str:expr, $x:expr] => {{
        $str += &$x;
    }};

    [] => {{}};
}

macro_rules! format_flags {
    ($flags:expr => $ty:ty) => {
        match <$ty>::from_bits($flags as _) {
            Some(flag) => format!("{flag:?}"),
            None => format!("(unknown)"),
        }
    };
}

#[repr(transparent)]
struct IoVec(libc::iovec);

impl fmt::Debug for IoVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let base = format_ptr(self.0.iov_base as u64);

        f.write_fmt(format_args!("{{base: {base}, ",))?;
        f.write_fmt(format_args!("len: {:?}}}", self.0.iov_len))
    }
}

#[repr(transparent)]
struct Fd(c_int);

impl fmt::Debug for Fd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_fd(self.0 as u64))
    }
}

#[repr(transparent)]
struct PollFd(libc::pollfd);

impl fmt::Debug for PollFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{{fd: {:?}, ", Fd(self.0.fd)))?;

        match nix::poll::PollFlags::from_bits(self.0.events) {
            Some(events) => f.write_fmt(format_args!("events: {events:?}}}")),
            None => f.write_fmt(format_args!("events: {}}}", self.0.events)),
        }
    }
}

fn format_ptr(addr: u64) -> String {
    if addr == 0 {
        "NULL".to_string()
    } else {
        format!("{addr:#x}")
    }
}

fn format_fd(fd: u64) -> String {
    match fd as c_int {
        0 => "stdin".to_string(),
        1 => "stdout".to_string(),
        2 => "stderr".to_string(),
        n => n.to_string(),
    }
}

fn format_fdset(proc: &mut Tracee, addr: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let mut fdset = nix::sys::select::FdSet::new();
    unsafe {
        let read_op = ReadMemory::new(proc).read(&mut fdset, addr as usize).apply();

        if read_op.is_err() {
            return "???".to_string();
        }
    }

    let fdset: Vec<_> = fdset.fds(None).map(|fd| Fd(fd.as_raw_fd())).collect();
    format!("{fdset:?}")
}

/// Try to read 20 bytes.
fn format_bytes_u8(proc: &mut Tracee, addr: u64, len: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let count = std::cmp::min(len as usize, 20);
    let mut bytes = Vec::<u8>::with_capacity(count);
    unsafe {
        let read_op = ReadMemory::new(proc)
            .read_slice(bytes.spare_capacity_mut(), addr as usize)
            .apply();

        match read_op {
            Ok(()) => bytes.set_len(count),
            Err(Error::IncompleteRead { read, .. }) => bytes.set_len(read),
            Err(_) => return "???".to_string(),
        }
    }

    // if the bytes are valid utf8, return that instead
    if let Ok(utf8) = std::str::from_utf8(&bytes) {
        return format!("b\"{utf8}\"");
    }

    format!("{bytes:x?}")
}

/// Read first 20 elements of an array of type `T`.
fn format_array<T: std::fmt::Debug>(proc: &mut Tracee, addr: u64, len: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let count = std::cmp::min(len as usize, 20);
    let mut values = Vec::<T>::with_capacity(count);
    unsafe {
        let read_op = ReadMemory::new(proc)
            .read_slice(values.spare_capacity_mut(), addr as usize)
            .apply();

        match read_op {
            Ok(()) => values.set_len(count),
            Err(Error::IncompleteRead { read, .. }) => {
                // truncate the values read buffer to those we read successfully
                let values_read = read / std::mem::size_of::<T>();
                values.set_len(values_read);
            }
            Err(_) => return "???".to_string(),
        }
    }

    format!("{values:?}")
}

/// Try to read a string with a known length and print the first 60 characters.
fn format_str(proc: &mut Tracee, addr: u64, len: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let count = std::cmp::min(len as usize, 60 * size_of::<char>());
    let mut bytes = Vec::<u8>::with_capacity(count);
    unsafe {
        let read_op = ReadMemory::new(proc)
            .read_slice(bytes.spare_capacity_mut(), addr as usize)
            .apply();

        match read_op {
            Ok(()) => bytes.set_len(count),
            Err(Error::IncompleteRead { read, .. }) => bytes.set_len(read),
            Err(_) => return "???".to_string(),
        }
    }

    let mut data = String::from_utf8_lossy(&bytes).into_owned().escape_default().to_string();

    if data.len() > 60 {
        data.truncate(57);
        data += "..";
    }

    format!("\"{data}\"")
}

/// Try to read a null terminated string.
pub fn read_c_str(proc: &mut Tracee, addr: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    // sorta arbitrary buffer size but we'll read as much as we can
    // this should still be faster than repeatedly reading one byte searching for a terminator
    let mut bytes = vec![0; *PAGE_SIZE / 2];
    unsafe {
        let read_op = ReadMemory::new(proc).read_slice(&mut bytes, addr as usize).apply();

        match read_op {
            Err(Error::IncompleteRead { read, .. }) => bytes.truncate(read),
            Err(_) => return "???".to_string(),
            Ok(()) => {}
        }
    }

    // find the first null terminator and add one if necessary
    match bytes.iter().position(|&b| b == b'\0') {
        Some(terminator) => bytes.truncate(terminator + 1),
        None => {
            bytes.pop();
            bytes.push(b'\0')
        }
    }

    // convert the cstring
    match CString::from_vec_with_nul(bytes) {
        Ok(data) => data.to_string_lossy().into_owned(),
        Err(..) => "???".to_string(),
    }
}

/// Try to read and format a null terminated string, printing the first 60 characters.
pub fn format_c_str(proc: &mut Tracee, addr: u64) -> String {
    let mut data = read_c_str(proc, addr).escape_default().to_string();
    if data.len() > 60 {
        data.truncate(57);
        data += "..";
    }

    format!("\"{data}\"")
}

// FIXME: i don't think this is being interpreted correctly
fn format_sigset(proc: &mut Tracee, addr: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let mut sigset = signal::SigSet::empty();

    unsafe {
        let read_op = ReadMemory::new(proc).read(&mut sigset, addr as usize).apply();

        if read_op.is_err() {
            return "???".to_string();
        }
    }

    if sigset == signal::SigSet::all() {
        return "~[]".to_string();
    }

    let signals: Vec<signal::Signal> = sigset.iter().collect();

    // if all 31 signals are set, it must be an empty set mask
    match signals.len() {
        31 => "~[]".to_string(),
        _ => format!("{signals:?}"),
    }
}

fn format_sigaction(proc: &mut Tracee, addr: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let mut sigaction = MaybeUninit::<signal::SigAction>::uninit();
    unsafe {
        let read_op = ReadMemory::new(proc).read(&mut sigaction, addr as usize).apply();

        if read_op.is_err() {
            return "???".to_string();
        }
    }

    let sigaction = unsafe { sigaction.assume_init() };
    let handler = sigaction.handler();
    let flags = sigaction.flags();
    let mask = sigaction.mask();
    let mask: Vec<signal::Signal> = mask.iter().collect();

    format!("{{sa_handler: {handler:?}, sa_mask: {mask:?}, sa_flags: {flags:?}}}")
}

fn format_futex_op(op: u64) -> &'static str {
    let op = op as c_int;

    if op & libc::FUTEX_PRIVATE_FLAG == libc::FUTEX_PRIVATE_FLAG {
        match op - libc::FUTEX_PRIVATE_FLAG {
            libc::FUTEX_WAIT => "FUTEX_WAIT_PRIVATE",
            libc::FUTEX_WAKE => "FUTEX_WAKE_PRIVATE",
            libc::FUTEX_FD => "FUTEX_FD_PRIVATE",
            libc::FUTEX_REQUEUE => "FUTEX_REQUEUE_PRIVATE",
            libc::FUTEX_CMP_REQUEUE => "FUTEX_CMP_REQUEUE_PRIVATE",
            _ => "(unknown)",
        }
    } else {
        match op {
            libc::FUTEX_WAIT => "FUTEX_WAIT",
            libc::FUTEX_WAKE => "FUTEX_WAKE",
            libc::FUTEX_FD => "FUTEX_FD",
            libc::FUTEX_REQUEUE => "FUTEX_REQUEUE",
            libc::FUTEX_CMP_REQUEUE => "FUTEX_CMP_REQUEUE",
            _ => "(unknown)",
        }
    }
}

fn format_stat(proc: &mut Tracee, addr: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let mut stats = MaybeUninit::<stat::FileStat>::uninit();
    unsafe {
        let read_op = ReadMemory::new(proc).read(&mut stats, addr as usize).apply();

        if read_op.is_err() {
            return "???".to_string();
        }
    }

    let stats = unsafe { stats.assume_init() };
    let mode = stats.st_mode;
    let size = stats.st_size;

    format!("{{st_mode={mode:?}, st_size={size}, ..}}")
}

fn format_ioctl(request: u64) -> &'static str {
    // don't ask me why we only take the first 15 bits, I don't know
    //
    // FIXME: figure out what's happening here
    let request = request as u32 & 0b111111111111111;

    if cfg!(target_arch = "x86_64") {
        let search = crate::ioctl::ARCH_CODES.iter().find(|(_, code)| code == &request);
        if let Some((name, _)) = search {
            return name;
        }
    }

    let search = crate::ioctl::GENERIC_CODES.iter().find(|(_, code)| code == &request);
    if let Some((name, _)) = search {
        return name;
    }

    "???"
}

fn format_timespec(proc: &mut Tracee, addr: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let mut time = nix::sys::time::TimeSpec::new(0, 0);
    unsafe {
        let read_op = ReadMemory::new(proc).read(&mut time, addr as usize).apply();

        if read_op.is_err() {
            return "???".to_string();
        }
    }

    let duration = std::time::Duration::from(time);
    format!("{duration:#?}")
}

fn format_timerval(proc: &mut Tracee, addr: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let mut time = nix::sys::time::TimeVal::new(0, 0);
    unsafe {
        let read_op = ReadMemory::new(proc).read(&mut time, addr as usize).apply();

        if read_op.is_err() {
            return "???".to_string();
        }
    }

    format!("{time}")
}

fn format_itimerval(proc: &mut Tracee, addr: u64) -> String {
    if addr == 0 {
        return "NULL".to_string();
    }

    let interval = format_timerval(proc, addr);
    let next = format_timerval(proc, addr + size_of::<nix::sys::time::TimeVal>() as u64);

    format!("{{interval: {interval}, next: {next}}}")
}

fn format_sockaddr(proc: &mut Tracee, addr: u64, socketlen: Option<u32>) -> String {
    let addr = addr as usize;

    if addr == 0 {
        return "NULL".to_string();
    }

    // read the first field of any sockaddr struct, it includes what family of addresses we
    // are working with
    let mut family = libc::sa_family_t::default() as i32;
    unsafe {
        let read_op = ReadMemory::new(proc).read(&mut family, addr).apply();

        if read_op.is_err() {
            return "???".to_string();
        }
    }

    let addr_family = {
        if family == libc::AF_UNSPEC {
            return "(opaque)".to_string();
        }

        match socket::AddressFamily::from_i32(family) {
            Some(family) => family,
            None => return "(unknown address family)".to_string(),
        }
    };

    match addr_family {
        // struct sockaddr_in
        socket::AddressFamily::Inet => unsafe {
            let mut sock_addr = MaybeUninit::<socket::sockaddr_in>::uninit();
            let read_op = ReadMemory::new(proc).read(&mut sock_addr, addr).apply();

            if read_op.is_err() {
                return "???".to_string();
            }

            let sock_addr = sock_addr.assume_init();
            let addr = std::net::Ipv4Addr::from(sock_addr.sin_addr.s_addr);
            let port = sock_addr.sin_port;

            format!("{{addr: {addr}, port: {port}}}")
        },
        // struct sockaddr_in6
        socket::AddressFamily::Inet6 => unsafe {
            let mut sock_addr = MaybeUninit::<socket::sockaddr_in6>::uninit();
            let read_op = ReadMemory::new(proc).read(&mut sock_addr, addr).apply();

            if read_op.is_err() {
                return "???".to_string();
            }

            let sock_addr = sock_addr.assume_init();
            let addr = std::net::Ipv6Addr::from(sock_addr.sin6_addr.s6_addr);
            let port = sock_addr.sin6_port;

            format!("{{addr: {addr}, port: {port}}}")
        },
        // struct sockaddr_un
        socket::AddressFamily::Unix => unsafe {
            let mut sock_addr = MaybeUninit::<socket::sockaddr>::uninit();
            let read_op = ReadMemory::new(proc).read(&mut sock_addr, addr).apply();

            if read_op.is_err() {
                return "???".to_string();
            }

            let sock_addr = sock_addr.assume_init();
            let unix_addr = match socket::UnixAddr::from_raw(&sock_addr, socketlen) {
                Some(addr) => addr,
                None => return "???".to_string(),
            };

            match unix_addr.path() {
                Some(path) => format!("{{path: {path:#?}}}"),
                None => "???".to_string(),
            }
        },
        // struct sockaddr_nl
        socket::AddressFamily::Netlink => unsafe {
            let mut netlink_addr = MaybeUninit::<socket::NetlinkAddr>::uninit();
            let read_op = ReadMemory::new(proc).read(&mut netlink_addr, addr).apply();

            if read_op.is_err() {
                return "???".to_string();
            }

            let netlink_addr = netlink_addr.assume_init();
            let pid = netlink_addr.pid();
            let groups = netlink_addr.groups();

            format!("{{pid: {pid}, groups: {groups}}}")
        },
        // struct sockaddr_alg
        socket::AddressFamily::Alg => unsafe {
            let mut alg_addr = MaybeUninit::<socket::AlgAddr>::uninit();
            let read_op = ReadMemory::new(proc).read(&mut alg_addr, addr).apply();

            if read_op.is_err() {
                return "???".to_string();
            }

            let alg_addr = alg_addr.assume_init();
            let tipe = alg_addr.alg_type().to_string_lossy();
            let name = alg_addr.alg_name().to_string_lossy();

            format!("{{type: {tipe}, name: {name}}}")
        },
        // struct sockaddr_ll
        socket::AddressFamily::Packet => unsafe {
            let mut link_addr = MaybeUninit::<socket::LinkAddr>::uninit();
            let read_op = ReadMemory::new(proc).read(&mut link_addr, addr).apply();

            if read_op.is_err() {
                return "???".to_string();
            }

            let link_addr = link_addr.assume_init();
            let protocol = link_addr.protocol();
            let iface = link_addr.ifindex();

            match link_addr.addr() {
                Some(mac) => {
                    let mac = format!(
                        "{:<02X}:{:<02X}:{:<02X}:{:<02X}:{:<02X}:{:<02X}",
                        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                    );

                    format!("{{protocol: {protocol}, iface: {iface}, mac: {mac}}}")
                }
                None => format!("{{protocol: {protocol}, iface: {iface}}}"),
            }
        },
        // struct sockaddr_vm
        socket::AddressFamily::Vsock => unsafe {
            let mut vsock_addr = MaybeUninit::<socket::VsockAddr>::uninit();
            let read_op = ReadMemory::new(proc).read(&mut vsock_addr, addr).apply();

            if read_op.is_err() {
                return "???".to_string();
            }

            let vsock_addr = vsock_addr.assume_init();
            let cid = vsock_addr.cid();
            let port = vsock_addr.port();

            format!("{{cid: {cid}, port: {port}}}")
        },
        _ => "(unknown address family)".to_string(),
    }
}

/// `sock_addr` is a *const sockaddr and `len_addr` is a *const u32.
fn format_sockaddr_using_len(proc: &mut Tracee, sock_addr: u64, len_addr: u64) -> String {
    if len_addr == 0 {
        return format_sockaddr(proc, sock_addr, None);
    }

    let mut len = 0u32;
    unsafe {
        let read_op = ReadMemory::new(proc).read(&mut len, len_addr as usize).apply();

        if read_op.is_err() {
            return "???".to_string();
        }
    }

    format_sockaddr(proc, sock_addr, Some(len))
}

fn format_sock_protocol(protocol: u64) -> &'static str {
    match protocol as c_int {
        libc::IPPROTO_TCP => "Tcp",
        libc::IPPROTO_UDP => "Udp",
        libc::IPPROTO_RAW => "Ip",
        libc::NETLINK_ROUTE => "NetlinkRoute",
        libc::NETLINK_USERSOCK => "NetlinkUsersock",
        libc::NETLINK_SOCK_DIAG => "NetlinkSockDiag",
        libc::NETLINK_SELINUX => "NetlinkSELINUX",
        libc::NETLINK_ISCSI => "NetlinkISCSI",
        libc::NETLINK_AUDIT => "NetlinkAudit",
        libc::NETLINK_FIB_LOOKUP => "NetlinkFIBLookup",
        libc::NETLINK_NETFILTER => "NetlinkNetfilter",
        libc::NETLINK_SCSITRANSPORT => "NetlinkSCSITransport",
        libc::NETLINK_RDMA => "NetlinkRDMA",
        libc::NETLINK_IP6_FW => "NetlinkIpv6Firewall",
        libc::NETLINK_DNRTMSG => "NetlinkDECNetroutingMsg",
        libc::NETLINK_KOBJECT_UEVENT => "NetlinkKObjectUEvent",
        libc::NETLINK_CRYPTO => "NetlinkCrypto",
        ETH_ALL => "EthAll",
        _ => "(unknown)",
    }
}

fn format_msghdr(proc: &mut Tracee, addr: u64) -> String {
    let mut msghdr = MaybeUninit::<libc::msghdr>::uninit();
    let msghdr = unsafe {
        let read_op = ReadMemory::new(proc).read(&mut msghdr, addr as usize).apply();

        if read_op.is_err() {
            return "???".to_string();
        }

        msghdr.assume_init()
    };

    let name = format_sockaddr(proc, msghdr.msg_name as u64, Some(msghdr.msg_namelen));
    let name_len = msghdr.msg_namelen;

    let msg_iov = format_array::<IoVec>(proc, msghdr.msg_iov as u64, msghdr.msg_iovlen as u64);
    let msg_iov_len = msghdr.msg_iovlen;

    let msg_ctrl = format_ptr(msghdr.msg_control as u64);
    let msg_ctrl_len = msghdr.msg_controllen;

    // ignore msg_flags as they don't appear to ever be set
    format!(
        "{{name: {name}, name_len: {name_len}, msg_iov: {msg_iov}, msg_iov_len: {msg_iov_len}, \
             msg_ctrl: {msg_ctrl}, msg_ctrl_len: {msg_ctrl_len}"
    )
}

fn format_socklevel(level: u64) -> &'static str {
    match level as c_int {
        libc::SOL_SOCKET => "SOL_SOCKET",
        libc::IPPROTO_TCP => "IPPROTO_TCP",
        libc::IPPROTO_IP => "IPPROTO_IP",
        libc::IPPROTO_IPV6 => "IPPROTO_IPV6",
        libc::SO_TYPE => "SO_TYPE",
        libc::SOL_UDP => "SOL_UDP",
        _ => "(unknown)",
    }
}

// there are probably a few missing here, but it includes all the ones
// I've found in the wild and some more
fn format_sockoptname(optname: u64) -> &'static str {
    match optname as c_int {
        libc::IP6T_SO_ORIGINAL_DST => "IP6T_SO_ORIGINAL_DST",
        libc::IPV6_DONTFRAG => "IPV6_DONTFRAG",
        libc::IPV6_RECVERR => "IPV6_RECVERR",
        libc::IPV6_RECVPKTINFO => "IPV6_RECVPKTINFO",
        libc::IPV6_TCLASS => "IPV6_TCLASS",
        libc::IPV6_UNICAST_HOPS => "IPV6_UNICAST_HOPS",
        libc::IPV6_V6ONLY => "IPV6_V6ONLY",
        libc::IP_DROP_MEMBERSHIP => "IP_DROP_MEMBERSHIP",
        libc::IP_MTU => "IP_MTU",
        libc::IP_RECVERR => "IP_RECVERR",
        libc::IP_TOS => "IP_TOS",
        libc::IP_TRANSPARENT => "IP_TRANSPARENT",
        libc::SO_ACCEPTCONN => "SO_ACCEPTCONN",
        libc::SO_BROADCAST => "SO_BROADCAST",
        libc::SO_DONTROUTE => "SO_DONTROUTE",
        libc::SO_ERROR => "SO_ERROR",
        libc::SO_KEEPALIVE => "SO_KEEPALIVE",
        libc::SO_LINGER => "SO_LINGER",
        libc::SO_OOBINLINE => "SO_OOBINLINE",
        libc::SO_PEERCRED => "SO_PEERCRED",
        libc::SO_PRIORITY => "SO_PRIORITY",
        libc::SO_RCVBUF => "SO_RCVBUF",
        libc::SO_RCVBUFFORCE => "SO_RCVBUFFORCE",
        libc::SO_RCVTIMEO => "SO_RCVTIMEO",
        libc::SO_REUSEADDR => "SO_REUSEADDR",
        libc::SO_REUSEPORT => "SO_REUSEPORT",
        libc::SO_RXQ_OVFL => "SO_RXQ_OVFL",
        libc::SO_SNDBUF => "SO_SNDBUF",
        libc::SO_SNDBUFFORCE => "SO_SNDBUFFORCE",
        libc::SO_SNDTIMEO => "SO_SNDTIMEO",
        libc::SO_TIMESTAMP => "SO_TIMESTAMP",
        libc::SO_TIMESTAMPING => "SO_TIMESTAMPING",
        libc::SO_TIMESTAMPNS => "SO_TIMESTAMPNS",
        libc::SO_TXTIME => "SO_TXTIME",
        libc::SO_TYPE => "SO_TYPE",
        libc::TCP_USER_TIMEOUT => "TCP_USER_TIMEOUT",
        libc::UDP_GRO => "UDP_GRO",
        libc::UDP_SEGMENT => "UDP_SEGMENT",
        _ => "(unknown)",
    }
}

/// Format arrays like argv and envp that include are made of an array of pointers
/// where the last element is a null pointer.
fn format_nullable_args(proc: &mut Tracee, addr: u64) -> String {
    let mut args = Vec::new();
    let max_args = 5;

    // only try to read the first `max_args` args
    for idx in 0..max_args {
        // ptr to c string (entry in array)
        let mut ptr = 0usize;

        unsafe {
            let addr = addr + (idx * std::mem::size_of::<*const i8>()) as u64;
            let read_op = ReadMemory::new(proc).read(&mut ptr, addr as usize).apply();

            if read_op.is_err() || ptr == 0 {
                break;
            }
        }

        let mut data = read_c_str(proc, ptr as u64).escape_default().to_string();
        if data.len() > 60 {
            data.truncate(57);
            data += "..";
        }

        args.push(data);
    }

    let arg_count = args.len();
    let mut args = format!("{args:?}");
    if arg_count == max_args {
        args.pop();
        args += ", ..]";
    }

    args
}

pub fn decode(tracee: &mut Tracee, syscall: c_long, args: [u64; 6]) -> String {
    let mut func = String::new();

    func += &syscall.to_string();
    func += "(";

    match syscall {
        libc::SYS_read => print_delimited![
            func,
            format_fd(args[0]),
            format_str(tracee, args[1], args[2]),
            args[2].to_string()
        ],
        libc::SYS_write => print_delimited![
            func,
            format_fd(args[0]),
            format_str(tracee, args[1], args[2]),
            args[2].to_string()
        ],
        libc::SYS_open => print_delimited![
            func,
            format_c_str(tracee, args[0]),
            format_flags!(args[1] => nix::fcntl::OFlag)
        ],
        libc::SYS_close => print_delimited![func, format_fd(args[0])],
        libc::SYS_stat => print_delimited![func, format_c_str(tracee, args[0]), format_ptr(args[1])],
        libc::SYS_fstat => print_delimited![func, format_fd(args[0]), format_ptr(args[1])],
        libc::SYS_lstat => {
            print_delimited![func, format_c_str(tracee, args[0]), format_ptr(args[1])]
        }
        libc::SYS_poll => print_delimited![
            func,
            format_array::<PollFd>(tracee, args[0], args[1]),
            args[1].to_string(),
            (args[2] as c_int).to_string()
        ],
        libc::SYS_lseek => print_delimited![
            func,
            format_fd(args[0]),
            (args[1] as i64).to_string(),
            match args[2] as c_int {
                libc::SEEK_SET => "SEEK_SET",
                libc::SEEK_CUR => "SEEK_CUR",
                libc::SEEK_END => "SEEK_END",
                libc::SEEK_DATA => "SEEK_DATA",
                libc::SEEK_HOLE => "SEEK_HOLE",
                _ => "(unknown)",
            }
        ],
        libc::SYS_mmap => print_delimited![
            func,
            format_ptr(args[0]),
            args[1].to_string(),
            format_flags!(args[2] => nix::sys::mman::ProtFlags),
            format_flags!(args[3] => nix::sys::mman::MapFlags),
            format_fd(args[4]),
            args[5].to_string()
        ],
        libc::SYS_mprotect => print_delimited![
            func,
            format_ptr(args[0]),
            args[1].to_string(),
            format_flags!(args[2] => nix::sys::mman::ProtFlags)
        ],
        libc::SYS_munmap => {
            print_delimited![func, format!("{:x}", args[0]), &args[1].to_string()]
        }
        libc::SYS_brk => print_delimited![func, format_ptr(args[0])],
        libc::SYS_rt_sigaction => print_delimited![
            func,
            match signal::Signal::try_from(args[0] as c_int) {
                Ok(s) => s.as_str(),
                Err(..) => "(unknown)",
            },
            format_sigaction(tracee, args[1]),
            format_sigaction(tracee, args[2])
        ],
        libc::SYS_rt_sigprocmask => print_delimited![
            func,
            match args[0] as c_int {
                libc::SIG_BLOCK => "SIG_BLOCK",
                libc::SIG_UNBLOCK => "SIG_UNBLOCK",
                libc::SIG_SETMASK => "SIG_SETMASK",
                _ => "(unknown)",
            },
            format_sigset(tracee, args[1]),
            format_sigset(tracee, args[2])
        ],
        libc::SYS_rt_sigreturn => print_delimited![],
        libc::SYS_ioctl => print_delimited![
            func,
            format_fd(args[0]),
            format_ioctl(args[1]),
            format_ptr(args[2])
        ],
        libc::SYS_pread64 => print_delimited![
            func,
            format_fd(args[0]),
            format_str(tracee, args[1], args[2]),
            args[2].to_string(),
            (args[3] as i64).to_string()
        ],
        libc::SYS_pwrite64 => print_delimited![
            func,
            format_fd(args[0]),
            format_str(tracee, args[1], args[2]),
            args[2].to_string(),
            (args[3] as i64).to_string()
        ],
        libc::SYS_readv => print_delimited![
            func,
            format_fd(args[0]),
            format_array::<IoVec>(tracee, args[1], args[2]),
            args[2].to_string()
        ],
        libc::SYS_writev => print_delimited![
            func,
            format_fd(args[0]),
            format_array::<IoVec>(tracee, args[1], args[2]),
            args[2].to_string()
        ],
        libc::SYS_access => print_delimited![
            func,
            format_c_str(tracee, args[0]),
            format_flags!(args[1] => nix::unistd::AccessFlags)
        ],
        libc::SYS_pipe => print_delimited![func, format_array::<Fd>(tracee, args[0], 2)],
        libc::SYS_pipe2 => print_delimited![
            func,
            format_array::<Fd>(tracee, args[0], 2),
            format_flags!(args[1] => nix::fcntl::OFlag)
        ],
        libc::SYS_select => print_delimited![
            func,
            args[0].to_string(),
            format_fdset(tracee, args[1]),
            format_fdset(tracee, args[2]),
            format_fdset(tracee, args[3]),
            format_ptr(args[4])
        ],
        libc::SYS_pselect6 => print_delimited![
            func,
            args[0].to_string(),
            format_fdset(tracee, args[1]),
            format_fdset(tracee, args[2]),
            format_fdset(tracee, args[3]),
            format_ptr(args[4]),
            format_sigset(tracee, args[5])
        ],
        libc::SYS_sched_yield => print_delimited![],
        libc::SYS_mremap => {
            if args[3] as i32 & libc::MREMAP_FIXED == libc::MREMAP_FIXED {
                print_delimited![
                    func,
                    format_ptr(args[0]),
                    args[1].to_string(),
                    args[2].to_string(),
                    format_flags!(args[3] => nix::sys::mman::MRemapFlags),
                    format_ptr(args[4])
                ]
            } else {
                print_delimited![
                    func,
                    format_ptr(args[0]),
                    args[1].to_string(),
                    args[2].to_string(),
                    format_flags!(args[3] => nix::sys::mman::MRemapFlags)
                ]
            }
        }
        libc::SYS_msync => print_delimited![
            func,
            format_ptr(args[0]),
            args[1].to_string(),
            format_flags!(args[2] => nix::sys::mman::MsFlags)
        ],
        libc::SYS_mincore => print_delimited![
            func,
            format_ptr(args[0]),
            args[1].to_string(),
            format_bytes_u8(tracee, args[2], args[1])
        ],
        libc::SYS_madvise => print_delimited![
            func,
            format_ptr(args[0]),
            args[1].to_string(),
            match args[2] as c_int {
                libc::MADV_NORMAL => "MADV_NORMAL",
                libc::MADV_RANDOM => "MADV_RANDOM",
                libc::MADV_SEQUENTIAL => "MADV_SEQUENTIAL",
                libc::MADV_WILLNEED => "MADV_WILLNEED",
                libc::MADV_DONTNEED => "MADV_DONTNEED",
                libc::MADV_REMOVE => "MADV_REMOVE",
                libc::MADV_DONTFORK => "MADV_DONTFORK",
                libc::MADV_DOFORK => "MADV_DOFORK",
                libc::MADV_HWPOISON => "MADV_HWPOISON",
                libc::MADV_MERGEABLE => "MADV_MERGEABLE",
                libc::MADV_UNMERGEABLE => "MADV_UNMERGEABLE",
                libc::MADV_SOFT_OFFLINE => "MADV_SOFT_OFFLINE",
                libc::MADV_HUGEPAGE => "MADV_HUGEPAGE",
                libc::MADV_NOHUGEPAGE => "MADV_NOHUGEPAGE",
                libc::MADV_DONTDUMP => "MADV_DONTDUMP",
                libc::MADV_DODUMP => "MADV_DODUMP",
                libc::MADV_FREE => "MADV_FREE",
                _ => "(unknown)",
            }
        ],
        libc::SYS_shmget => print_delimited![
            func,
            (args[0] as c_int).to_string(),
            args[1].to_string(),
            // TODO: print shmflg
            args[2].to_string()
        ],
        libc::SYS_shmat => print_delimited![
            func,
            // TODO: print shmid
            args[0].to_string(),
            format_ptr(args[1]),
            // TODO: print shmflg
            args[0].to_string()
        ],
        libc::SYS_shmctl => print_delimited![
            func,
            // TODO: print shmid
            args[0].to_string(),
            match args[1] as c_int {
                libc::IPC_RMID => "IPC_RMID",
                libc::IPC_SET => "IPC_SET",
                libc::IPC_STAT => "IPC_STAT",
                libc::IPC_INFO => "IPC_INFO",
                _ => "(unknown)",
            },
            format_ptr(args[2])
        ],
        libc::SYS_dup => print_delimited![func, format_fd(args[0])],
        libc::SYS_dup2 => print_delimited![func, format_fd(args[0]), format_fd(args[0])],
        libc::SYS_pause => print_delimited![],
        libc::SYS_nanosleep => {
            print_delimited![func, format_timespec(tracee, args[0]), format_ptr(args[1])]
        }
        libc::SYS_getitimer => print_delimited![
            func,
            match args[0] as c_int {
                libc::ITIMER_REAL => "ITIMER_REAL",
                libc::ITIMER_VIRTUAL => "ITIMER_VIRUAL",
                libc::ITIMER_PROF => "ITIMER_PROF",
                _ => "(unknown)",
            },
            format_itimerval(tracee, args[1])
        ],
        libc::SYS_alarm => print_delimited![func, args[0].to_string()],
        libc::SYS_setitimer => print_delimited![
            func,
            match args[0] as c_int {
                libc::ITIMER_REAL => "ITIMER_REAL",
                libc::ITIMER_VIRTUAL => "ITIMER_VIRUAL",
                libc::ITIMER_PROF => "ITIMER_PROF",
                _ => "(unknown)",
            },
            format_itimerval(tracee, args[1]),
            format_itimerval(tracee, args[2])
        ],
        libc::SYS_getpid => print_delimited![],
        libc::SYS_sendfile => print_delimited![
            func,
            format_fd(args[0]),
            format_fd(args[1]),
            format_ptr(args[2]),
            args[3].to_string()
        ],
        libc::SYS_socket => print_delimited![
            func,
            match socket::AddressFamily::from_i32(args[0] as i32) {
                Some(s) => format!("{s:?}"),
                None => "(unknown)".to_string(),
            },
            match socket::SockType::try_from(args[1] as i32) {
                Ok(s) => format!("{s:?}"),
                Err(..) => "(unknown)".to_string(),
            },
            format_sock_protocol(args[2])
        ],
        libc::SYS_connect => print_delimited![
            func,
            format_fd(args[0]),
            format_sockaddr(tracee, args[1], Some(args[2] as u32)),
            args[2].to_string()
        ],
        libc::SYS_accept => print_delimited![
            func,
            format_fd(args[0]),
            format_sockaddr_using_len(tracee, args[1], args[2]),
            format_ptr(args[2])
        ],
        libc::SYS_sendto => print_delimited![
            func,
            format_fd(args[0]),
            format_bytes_u8(tracee, args[1], args[2]),
            args[2].to_string(),
            format_flags!(args[3] => nix::sys::socket::MsgFlags),
            format_sockaddr(tracee, args[4], Some(args[5] as u32)),
            args[5].to_string()
        ],
        libc::SYS_recvfrom => print_delimited![
            func,
            format_fd(args[0]),
            format_bytes_u8(tracee, args[1], args[2]),
            args[2].to_string(),
            format_flags!(args[3] => nix::sys::socket::MsgFlags),
            format_sockaddr_using_len(tracee, args[4], args[5]),
            format_ptr(args[5])
        ],
        libc::SYS_sendmsg => print_delimited![
            func,
            format_fd(args[0]),
            format_msghdr(tracee, args[1]),
            format_flags!(args[2] => nix::sys::socket::MsgFlags)
        ],
        libc::SYS_recvmsg => print_delimited![
            func,
            format_fd(args[0]),
            format_msghdr(tracee, args[1]),
            format_flags!(args[2] => nix::sys::socket::MsgFlags)
        ],
        libc::SYS_shutdown => print_delimited![
            func,
            format_fd(args[0]),
            match args[1] as c_int {
                libc::SHUT_RD => "SHUT_READ",
                libc::SHUT_WR => "SHUT_WRITE",
                libc::SHUT_RDWR => "SHUT_RW",
                _ => "(unknown)",
            }
        ],
        libc::SYS_bind => print_delimited![
            func,
            format_fd(args[0]),
            format_sockaddr(tracee, args[1], Some(args[2] as u32)),
            args[2].to_string()
        ],
        libc::SYS_listen => print_delimited![func, format_fd(args[0]), args[1].to_string()],
        libc::SYS_getsockname => print_delimited![
            func,
            format_fd(args[0]),
            format_sockaddr_using_len(tracee, args[1], args[2]),
            format_ptr(args[2])
        ],
        libc::SYS_getpeername => print_delimited![
            func,
            format_fd(args[0]),
            format_sockaddr_using_len(tracee, args[1], args[2]),
            format_ptr(args[2])
        ],
        libc::SYS_socketpair => print_delimited![
            func,
            match socket::AddressFamily::from_i32(args[0] as i32) {
                Some(family) => format!("{family:?}"),
                None => "(unknown address family)".to_string(),
            },
            match socket::SockType::try_from(args[1] as i32) {
                Ok(tipe) => format!("{tipe:?}"),
                Err(..) => "(unknown address family)".to_string(),
            },
            format_sock_protocol(args[2]),
            format_ptr(args[3])
        ],
        libc::SYS_setsockopt => print_delimited![
            func,
            format_fd(args[0]),
            format_socklevel(args[1]),
            format_sockoptname(args[2]),
            format_bytes_u8(tracee, args[3], args[4]),
            args[4].to_string()
        ],
        libc::SYS_getsockopt => print_delimited![
            func,
            format_fd(args[0]),
            format_socklevel(args[1]),
            format_sockoptname(args[2]),
            format_ptr(args[3]),
            format_ptr(args[3])
        ],
        libc::SYS_clone => print_delimited![
            func,
            format!(
                "flags: {}",
                format_flags!(args[0] & !0xff => nix::sched::CloneFlags)
            ),
            match signal::Signal::try_from((args[0] & 0xff) as c_int) {
                Ok(s) => format!("exit_signal: {s}"),
                Err(..) => "(unknown)".to_string(),
            },
            format!("child_stack: {}", format_ptr(args[1]))
        ],
        libc::SYS_fork => print_delimited![],
        libc::SYS_vfork => print_delimited![],
        libc::SYS_execve => print_delimited![
            func,
            format_c_str(tracee, args[0]),
            format_nullable_args(tracee, args[1]),
            format_nullable_args(tracee, args[2])
        ],
        libc::SYS_openat => print_delimited![
            func,
            if args[0] == 4294967196 {
                "AT_FDCWD".to_string()
            } else {
                format_fd(args[0])
            },
            format_c_str(tracee, args[1]),
            format_flags!(args[2] => nix::fcntl::OFlag)
        ],
        libc::SYS_set_tid_address => print_delimited![func, format_ptr(args[0])],
        libc::SYS_set_robust_list => {
            print_delimited![func, format_ptr(args[0]), args[1].to_string()]
        }
        libc::SYS_getrandom => print_delimited![
            func,
            format_bytes_u8(tracee, args[0], args[1]),
            args[1].to_string(),
            {
                let has_random = args[2] as u32 & libc::GRND_RANDOM == libc::GRND_RANDOM;
                let has_nonblock = args[2] as u32 & libc::GRND_NONBLOCK == libc::GRND_NONBLOCK;

                match (has_random, has_nonblock) {
                    (true, true) => "GRND_NONBLOCK | GRND_RANDOM",
                    (true, false) => "GRND_RANDOM",
                    (false, true) => "GRND_NONBLOCK",
                    _ => "(empty)",
                }
            }
        ],
        libc::SYS_newfstatat => print_delimited![
            func,
            format_fd(args[0]),
            format_c_str(tracee, args[1]),
            format_stat(tracee, args[2]),
            format_flags!(args[3] => nix::fcntl::AtFlags)
        ],
        libc::SYS_futex => print_delimited![
            func,
            format_ptr(args[0]),
            format_futex_op(args[1]),
            args[2].to_string()
        ],
        libc::SYS_getuid => print_delimited![],
        libc::SYS_syslog => print_delimited![
            func,
            match args[0] {
                0 => "SYSLOG_ACTION_CLOSE",
                1 => "SYSLOG_ACTION_OPEN",
                2 => "SYSLOG_ACTION_READ",
                3 => "SYSLOG_ACTION_READ_ALL",
                4 => "SYSLOG_ACTION_READ_CLEAR",
                5 => "SYSLOG_ACTION_CONSOLE_OFF",
                6 => "SYSLOG_ACTION_CONSOLE_ON",
                7 => "SYSLOG_ACTION_CONSOLE_LEVEL",
                8 => "SYSLOG_ACTION_SIZE_UNREAD",
                9 => "SYSLOG_ACTION_SIZE_BUFFER",
                _ => "(unknown)",
            },
            format_str(tracee, args[1], args[2])
        ],
        libc::SYS_getgid => print_delimited![],
        libc::SYS_setuid => print_delimited![func, args[0].to_string()],
        libc::SYS_setgid => print_delimited![func, args[0].to_string()],
        libc::SYS_geteuid => print_delimited![],
        libc::SYS_getegid => print_delimited![],
        libc::SYS_setpgid => print_delimited![func, args[0].to_string(), args[1].to_string()],
        libc::SYS_getppid => print_delimited![],
        libc::SYS_getpgrp => print_delimited![],
        libc::SYS_setsid => print_delimited![func, args[0].to_string()],
        libc::SYS_setreuid => print_delimited![func, args[0].to_string(), args[1].to_string()],
        libc::SYS_setregid => print_delimited![func, args[0].to_string(), args[1].to_string()],
        libc::SYS_getgroups => print_delimited![func, args[0].to_string(), format_ptr(args[1])],
        libc::SYS_setgroups => print_delimited![
            func,
            args[0].to_string(),
            format_array::<c_int>(tracee, args[1], args[0])
        ],
        libc::SYS_setresuid => print_delimited![
            func,
            args[0].to_string(),
            args[1].to_string(),
            args[2].to_string()
        ],
        libc::SYS_getresuid => print_delimited![
            func,
            format_ptr(args[0]),
            format_ptr(args[1]),
            format_ptr(args[2])
        ],
        libc::SYS_setresgid => print_delimited![
            func,
            args[0].to_string(),
            args[1].to_string(),
            args[2].to_string()
        ],
        libc::SYS_getresgid => print_delimited![
            func,
            format_ptr(args[0]),
            format_ptr(args[1]),
            format_ptr(args[2])
        ],
        libc::SYS_getpgid => print_delimited![func, args[0].to_string()],
        libc::SYS_setfsuid => print_delimited![func, args[0].to_string()],
        libc::SYS_setfsgid => print_delimited![func, args[0].to_string()],
        libc::SYS_getsid => print_delimited![func, args[0].to_string()],
        libc::SYS_exit => print_delimited![func, args[0].to_string()],
        libc::SYS_exit_group => print_delimited![func, args[0].to_string()],
        _ => func += "..",
    }

    func += ")";
    func
}
