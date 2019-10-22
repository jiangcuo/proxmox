use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::u32;

use failure::*;
use lazy_static::lazy_static;
use libc;
use regex::Regex;

use proxmox_tools::fs::file_read_firstline;

/// POSIX sysconf call
pub fn sysconf(name: i32) -> i64 {
    extern "C" {
        fn sysconf(name: i32) -> i64;
    }
    unsafe { sysconf(name) }
}

lazy_static! {
    static ref CLOCK_TICKS: f64 = sysconf(libc::_SC_CLK_TCK) as f64;
}

pub struct ProcFsPidStat {
    pub status: u8,
    pub utime: u64,
    pub stime: u64,
    pub starttime: u64,
    pub vsize: u64,
    pub rss: i64,
}

pub fn read_proc_pid_stat(pid: libc::pid_t) -> Result<ProcFsPidStat, Error> {
    parse_proc_pid_stat(
        pid,
        std::str::from_utf8(&std::fs::read(format!("/proc/{}/stat", pid))?)?,
    )
}

fn parse_proc_pid_stat(pid: libc::pid_t, statstr: &str) -> Result<ProcFsPidStat, Error> {
    lazy_static! {
        static ref REGEX: Regex = Regex::new(concat!(
            r"^(?P<pid>\d+) \(.*\) (?P<status>\S) -?\d+ -?\d+ -?\d+ -?\d+ -?\d+ \d+ \d+ \d+ \d+ \d+ ",
            r"(?P<utime>\d+) (?P<stime>\d+) -?\d+ -?\d+ -?\d+ -?\d+ -?\d+ 0 ",
            r"(?P<starttime>\d+) (?P<vsize>\d+) (?P<rss>-?\d+) ",
            r"\d+ \d+ \d+ \d+ \d+ \d+ \d+ \d+ \d+ \d+ \d+ \d+ \d+ -?\d+ -?\d+ \d+ \d+ \d+"
        )).unwrap();
    }

    if let Some(cap) = REGEX.captures(&statstr) {
        if pid != cap["pid"].parse::<i32>().unwrap() {
            bail!(
                "unable to read pid stat for process '{}' - got wrong pid",
                pid
            );
        }

        return Ok(ProcFsPidStat {
            status: cap["status"].as_bytes()[0],
            utime: cap["utime"].parse::<u64>().unwrap(),
            stime: cap["stime"].parse::<u64>().unwrap(),
            starttime: cap["starttime"].parse::<u64>().unwrap(),
            vsize: cap["vsize"].parse::<u64>().unwrap(),
            rss: cap["rss"].parse::<i64>().unwrap() * 4096,
        });
    }

    bail!("unable to read pid stat for process '{}'", pid);
}

#[test]
fn test_read_proc_pid_stat() {
    let stat = parse_proc_pid_stat(
        28900,
        "28900 (zsh) S 22489 28900 28900 34826 10252 4194304 6851 5946551 0 2344 6 3 25205 1413 \
         20 0 1 0 287592 12496896 1910 18446744073709551615 93999319244800 93999319938061 \
         140722897984224 0 0 0 2 3686404 134295555 1 0 0 17 10 0 0 0 0 0 93999320079088 \
         93999320108360 93999343271936 140722897992565 140722897992570 140722897992570 \
         140722897993707 0",
    )
    .expect("successful parsing of a sample /proc/PID/stat entry");
    assert_eq!(stat.status, b'S');
    assert_eq!(stat.utime, 6);
    assert_eq!(stat.stime, 3);
    assert_eq!(stat.starttime, 287592);
    assert_eq!(stat.vsize, 12496896);
    assert_eq!(stat.rss, 1910 * 4096);
}

pub fn read_proc_starttime(pid: libc::pid_t) -> Result<u64, Error> {
    let info = read_proc_pid_stat(pid)?;

    Ok(info.starttime)
}

pub fn check_process_running(pid: libc::pid_t) -> Option<ProcFsPidStat> {
    if let Ok(info) = read_proc_pid_stat(pid) {
        if info.status != b'Z' {
            return Some(info);
        }
    }
    None
}

pub fn check_process_running_pstart(pid: libc::pid_t, pstart: u64) -> Option<ProcFsPidStat> {
    if let Some(info) = check_process_running(pid) {
        if info.starttime == pstart {
            return Some(info);
        }
    }
    None
}

pub fn read_proc_uptime() -> Result<(f64, f64), Error> {
    let path = "/proc/uptime";
    let line = file_read_firstline(&path)?;
    let mut values = line.split_whitespace().map(|v| v.parse::<f64>());

    match (values.next(), values.next()) {
        (Some(Ok(up)), Some(Ok(idle))) => Ok((up, idle)),
        _ => bail!("Error while parsing '{}'", path),
    }
}

pub fn read_proc_uptime_ticks() -> Result<(u64, u64), Error> {
    let (mut up, mut idle) = read_proc_uptime()?;
    up *= *CLOCK_TICKS;
    idle *= *CLOCK_TICKS;
    Ok((up as u64, idle as u64))
}

#[derive(Debug)]
pub struct ProcFsMemInfo {
    pub memtotal: u64,
    pub memfree: u64,
    pub memused: u64,
    pub memshared: u64,
    pub swaptotal: u64,
    pub swapfree: u64,
    pub swapused: u64,
}

pub fn read_meminfo() -> Result<ProcFsMemInfo, Error> {
    let path = "/proc/meminfo";
    let file = OpenOptions::new().read(true).open(&path)?;

    let mut meminfo = ProcFsMemInfo {
        memtotal: 0,
        memfree: 0,
        memused: 0,
        memshared: 0,
        swaptotal: 0,
        swapfree: 0,
        swapused: 0,
    };

    let (mut buffers, mut cached) = (0, 0);
    for line in BufReader::new(&file).lines() {
        let content = line?;
        let mut content_iter = content.split_whitespace();
        if let (Some(key), Some(value)) = (content_iter.next(), content_iter.next()) {
            match key {
                "MemTotal:" => meminfo.memtotal = value.parse::<u64>()? * 1024,
                "MemFree:" => meminfo.memfree = value.parse::<u64>()? * 1024,
                "SwapTotal:" => meminfo.swaptotal = value.parse::<u64>()? * 1024,
                "SwapFree:" => meminfo.swapfree = value.parse::<u64>()? * 1024,
                "Buffers:" => buffers = value.parse::<u64>()? * 1024,
                "Cached:" => cached = value.parse::<u64>()? * 1024,
                _ => continue,
            }
        }
    }

    meminfo.memfree += buffers + cached;
    meminfo.memused = meminfo.memtotal - meminfo.memfree;

    meminfo.swapused = meminfo.swaptotal - meminfo.swapfree;

    let spages_line = file_read_firstline("/sys/kernel/mm/ksm/pages_sharing")?;
    meminfo.memshared = spages_line.trim_end().parse::<u64>()? * 4096;

    Ok(meminfo)
}

#[derive(Clone, Debug)]
pub struct ProcFsCPUInfo {
    pub user_hz: f64,
    pub mhz: f64,
    pub model: String,
    pub hvm: bool,
    pub sockets: usize,
    pub cpus: usize,
}

static CPU_INFO: Option<ProcFsCPUInfo> = None;

pub fn read_cpuinfo() -> Result<ProcFsCPUInfo, Error> {
    if let Some(cpu_info) = &CPU_INFO {
        return Ok(cpu_info.clone());
    }

    let path = "/proc/cpuinfo";
    let file = OpenOptions::new().read(true).open(&path)?;

    let mut cpuinfo = ProcFsCPUInfo {
        user_hz: *CLOCK_TICKS,
        mhz: 0.0,
        model: String::new(),
        hvm: false,
        sockets: 0,
        cpus: 0,
    };

    let mut socket_ids = HashSet::new();
    for line in BufReader::new(&file).lines() {
        let content = line?;
        if content.is_empty() {
            continue;
        }
        let mut content_iter = content.split(':');
        match (content_iter.next(), content_iter.next()) {
            (Some(key), Some(value)) => match key.trim_end() {
                "processor" => cpuinfo.cpus += 1,
                "model name" => cpuinfo.model = value.trim().to_string(),
                "cpu MHz" => cpuinfo.mhz = value.trim().parse::<f64>()?,
                "flags" => cpuinfo.hvm = value.contains(" vmx ") || value.contains(" svm "),
                "physical id" => {
                    let id = value.trim().parse::<u8>()?;
                    socket_ids.insert(id);
                }
                _ => continue,
            },
            _ => bail!("Error while parsing '{}'", path),
        }
    }
    cpuinfo.sockets = socket_ids.len();

    Ok(cpuinfo)
}

#[derive(Debug)]
pub struct ProcFsMemUsage {
    pub size: u64,
    pub resident: u64,
    pub shared: u64,
}

pub fn read_memory_usage() -> Result<ProcFsMemUsage, Error> {
    let path = format!("/proc/{}/statm", std::process::id());
    let line = file_read_firstline(&path)?;
    let mut values = line.split_whitespace().map(|v| v.parse::<u64>());

    let ps = 4096;
    match (values.next(), values.next(), values.next()) {
        (Some(Ok(size)), Some(Ok(resident)), Some(Ok(shared))) => Ok(ProcFsMemUsage {
            size: size * ps,
            resident: resident * ps,
            shared: shared * ps,
        }),
        _ => bail!("Error while parsing '{}'", path),
    }
}

#[derive(Debug)]
pub struct ProcFsNetDev {
    pub device: String,
    pub receive: u64,
    pub send: u64,
}

pub fn read_proc_net_dev() -> Result<Vec<ProcFsNetDev>, Error> {
    let path = "/proc/net/dev";
    let file = OpenOptions::new().read(true).open(&path)?;

    let mut result = Vec::new();
    for line in BufReader::new(&file).lines().skip(2) {
        let content = line?;
        let mut iter = content.split_whitespace();
        match (iter.next(), iter.next(), iter.nth(7)) {
            (Some(device), Some(receive), Some(send)) => {
                result.push(ProcFsNetDev {
                    device: device[..device.len() - 1].to_string(),
                    receive: receive.parse::<u64>()?,
                    send: send.parse::<u64>()?,
                });
            }
            _ => bail!("Error while parsing '{}'", path),
        }
    }

    Ok(result)
}

fn hex_nibble(c: u8) -> Result<u8, Error> {
    Ok(match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 0xa,
        b'A'..=b'F' => c - b'A' + 0xa,
        _ => bail!("not a hex digit: {}", c as char),
    })
}

fn hexstr_to_ipv4addr<T: AsRef<[u8]>>(hex: T) -> Result<Ipv4Addr, Error> {
    let hex = hex.as_ref();
    if hex.len() != 8 {
        bail!("Error while converting hex string to IPv4 address: unexpected string length");
    }

    let mut addr = [0u8; 4];
    for i in 0..4 {
        addr[3 - i] = (hex_nibble(hex[i * 2])? << 4) + hex_nibble(hex[i * 2 + 1])?;
    }

    Ok(Ipv4Addr::from(addr))
}

#[derive(Debug)]
pub struct ProcFsNetRoute {
    pub dest: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub metric: u32,
    pub mtu: u32,
    pub iface: String,
}

pub fn read_proc_net_route() -> Result<Vec<ProcFsNetRoute>, Error> {
    let path = "/proc/net/route";
    let file = OpenOptions::new().read(true).open(&path)?;

    let mut result = Vec::new();
    for line in BufReader::new(&file).lines().skip(1) {
        let content = line?;
        if content.is_empty() {
            continue;
        }
        let mut iter = content.split_whitespace();

        let mut next = || {
            iter.next()
                .ok_or_else(|| format_err!("Error while parsing '{}'", path))
        };

        let (iface, dest, gateway) = (next()?, next()?, next()?);
        for _ in 0..3 {
            next()?;
        }
        let (metric, mask, mtu) = (next()?, next()?, next()?);

        result.push(ProcFsNetRoute {
            dest: hexstr_to_ipv4addr(dest)?,
            gateway: hexstr_to_ipv4addr(gateway)?,
            mask: hexstr_to_ipv4addr(mask)?,
            metric: metric.parse()?,
            mtu: mtu.parse()?,
            iface: iface.to_string(),
        });
    }

    Ok(result)
}

fn hexstr_to_ipv6addr<T: AsRef<[u8]>>(hex: T) -> Result<Ipv6Addr, Error> {
    let hex = hex.as_ref();
    if hex.len() != 32 {
        bail!("Error while converting hex string to IPv6 address: unexpected string length");
    }

    let mut addr = std::mem::MaybeUninit::<[u8; 16]>::uninit();
    let addr = unsafe {
        let ap = &mut *addr.as_mut_ptr();
        for i in 0..16 {
            ap[i] = (hex_nibble(hex[i * 2])? << 4) + hex_nibble(hex[i * 2 + 1])?;
        }
        addr.assume_init()
    };

    Ok(Ipv6Addr::from(addr))
}

fn hexstr_to_u8<T: AsRef<[u8]>>(hex: T) -> Result<u8, Error> {
    let hex = hex.as_ref();
    if hex.len() != 2 {
        bail!("Error while converting hex string to u8: unexpected string length");
    }

    Ok((hex_nibble(hex[0])? << 4) + hex_nibble(hex[1])?)
}

fn hexstr_to_u32<T: AsRef<[u8]>>(hex: T) -> Result<u32, Error> {
    let hex = hex.as_ref();
    if hex.len() != 8 {
        bail!("Error while converting hex string to u32: unexpected string length");
    }

    let mut bytes = [0u8; 4];
    for i in 0..4 {
        bytes[i] = (hex_nibble(hex[i * 2])? << 4) + hex_nibble(hex[i * 2 + 1])?;
    }

    Ok(u32::from_be_bytes(bytes))
}

#[derive(Debug)]
pub struct ProcFsNetIPv6Route {
    pub dest: Ipv6Addr,
    pub prefix: u8,
    pub gateway: Ipv6Addr,
    pub metric: u32,
    pub iface: String,
}

pub fn read_proc_net_ipv6_route() -> Result<Vec<ProcFsNetIPv6Route>, Error> {
    let path = "/proc/net/ipv6_route";
    let file = OpenOptions::new().read(true).open(&path)?;

    let mut result = Vec::new();
    for line in BufReader::new(&file).lines() {
        let content = line?;
        if content.is_empty() {
            continue;
        }
        let mut iter = content.split_whitespace();

        let mut next = || {
            iter.next()
                .ok_or_else(|| format_err!("Error while parsing '{}'", path))
        };

        let (dest, prefix) = (next()?, next()?);
        for _ in 0..2 {
            next()?;
        }
        let (nexthop, metric) = (next()?, next()?);
        for _ in 0..3 {
            next()?;
        }
        let iface = next()?;

        result.push(ProcFsNetIPv6Route {
            dest: hexstr_to_ipv6addr(dest)?,
            prefix: hexstr_to_u8(prefix)?,
            gateway: hexstr_to_ipv6addr(nexthop)?,
            metric: hexstr_to_u32(metric)?,
            iface: iface.to_string(),
        });
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_proc_net_route() {
        read_proc_net_route().unwrap();
    }

    #[test]
    fn test_read_proc_net_ipv6_route() {
        read_proc_net_ipv6_route().unwrap();
    }
}
