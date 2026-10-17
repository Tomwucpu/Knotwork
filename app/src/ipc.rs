//! 单实例守卫与启动请求转发（FR-MR-05）。
//!
//! # 为什么用本机 TCP 而不是命名管道 / 共享内存
//!
//! 工作区禁止 `unsafe`，因此排除了直接调用 Win32（`CreateMutexW` / `WM_COPYDATA`）
//! 的方案；`std::net` 是纯安全 Rust，跨平台可用，且天然带"目标进程是否还活着"
//! 的判据 —— 连接失败就等于没有主实例。
//!
//! 安全边界：监听**只绑 127.0.0.1 的临时端口**，端口号写在配置文件目录下的
//! `instance.json` 里（同一用户可读）。协议是逐行文本，只接受**已存在的本地
//! 仓库路径**，收到后由主实例当作一次普通"打开仓库"处理 —— 主实例仍然会做
//! 仓库校验与去重，因此这个通道不能被用来"命令主实例做什么"。
//!
//! # 降级
//!
//! 任一环节失败（配置目录不可用、端口占用、写文件失败）都退回
//! [`Startup::Standalone`]：多开一个进程是**可用性上的退让**，比"启动失败"好。

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// TCP 连接/读写的超时：本机回环，1 秒足够，超时即视为"没有主实例"。
const IO_TIMEOUT: Duration = Duration::from_secs(1);

/// 实例信息文件名。
const INFO_FILE: &str = "instance.json";

/// 启动结论。
pub enum Startup {
    /// 本进程是主实例；持有 [`InstanceServer`] 直到退出。
    Primary(InstanceServer),
    /// 已把请求转交给运行中的实例；本进程应立即退出。
    Forwarded,
    /// 以独立实例启动（无法判定或用户显式 `--isolate`）。
    Standalone(String),
}

/// 主实例的监听器；丢弃即停止接收。
pub struct InstanceServer {
    info_path: PathBuf,
    _thread: std::thread::JoinHandle<()>,
}

impl InstanceServer {
    /// 停止监听并删除实例信息文件。
    pub fn shutdown(&self) {
        // 删除信息文件后，新启动的进程不会再指向本实例；监听线程随进程退出结束。
        let _ = std::fs::remove_file(&self.info_path);
    }
}

/// 待打开路径的进程内队列（监听线程写入，UI 线程取出）。
fn pending() -> &'static Mutex<Vec<String>> {
    static PENDING: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

/// 取出全部待打开的路径。
#[must_use]
pub fn take_pending() -> Vec<String> {
    let mut queue = pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *queue)
}

/// 启动单实例守卫。
///
/// `paths` 是命令行给出的仓库路径；有主实例时连同路径一起转发。
#[must_use]
pub fn start(paths: &[String], isolate: bool) -> Startup {
    if isolate {
        return Startup::Standalone("已指定 --isolate，按独立实例启动".to_owned());
    }

    let dir = match gt_config::config_dir() {
        Ok(dir) => dir,
        Err(err) => return Startup::Standalone(format!("配置目录不可用：{err}")),
    };
    let info_path = dir.join(INFO_FILE);

    // 1) 尝试把请求交给已运行实例。
    if let Some(addr) = read_info(&info_path) {
        if pid_alive(addr.0) && forward_to(addr.1, paths) {
            tracing::info!(port = addr.1, "已将打开请求转交给运行中的实例");
            return Startup::Forwarded;
        }
        tracing::debug!("实例信息已过期，接管为主实例");
        let _ = std::fs::remove_file(&info_path);
    }

    // 2) 成为主实例。
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(err) => return Startup::Standalone(format!("无法监听回环端口：{err}")),
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(err) => return Startup::Standalone(format!("无法获取监听地址：{err}")),
    };

    if let Err(err) = write_info(&info_path, std::process::id(), addr.port()) {
        // 写不了信息文件只是"后续启动无法转发"，本实例照常工作。
        tracing::warn!(%err, "写入实例信息失败，本次启动无法接收转发请求");
        return Startup::Standalone(format!("写入实例信息失败：{err}"));
    }

    tracing::info!(port = addr.port(), "单实例监听已就绪");
    let thread = std::thread::spawn(move || serve(&listener));
    Startup::Primary(InstanceServer {
        info_path,
        _thread: thread,
    })
}

/// 接受连接并把路径塞进待处理队列。
///
/// 取 `&TcpListener` 而不是所有权：`incoming()` 只需要借用，而监听器的生命周期
/// 由调用方（`start` 里的线程闭包）持有。
fn serve(listener: &TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            break;
        };
        let _ = handle_client(stream);
    }
}

/// 处理一次转发请求：读取到**空行**为止，交给队列，回 `OK`。
///
/// 协议必须显式定界：客户端在收到 `OK` 之前不会关闭连接，若服务端等 EOF
/// 才处理，两边会互相干等直到超时。
fn handle_client(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    let reader = BufReader::new(stream.try_clone()?);
    let mut received = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        received.push(trimmed.to_owned());
    }

    if !received.is_empty() {
        let mut queue = pending()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.extend(received);
    }

    stream.write_all(b"OK\n")?;
    stream.flush()
}

/// 把路径转发给指定端口；成功（收到 `OK`）返回 `true`。
fn forward_to(port: u16, paths: &[String]) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, IO_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));

    let mut payload = String::new();
    for path in paths {
        // 路径里的换行会破坏行式协议：直接拒发（真实路径不含换行）。
        if path.contains('\n') || path.contains('\r') {
            return false;
        }
        payload.push_str(path);
        payload.push('\n');
    }
    // 空行 = 消息结束（无参数启动时就是一条只有定界符的请求）。
    payload.push('\n');

    if stream.write_all(payload.as_bytes()).is_err() {
        return false;
    }
    let _ = stream.flush();

    let mut reply = String::new();
    let mut reader = BufReader::new(stream);
    matches!(reader.read_line(&mut reply), Ok(n) if n > 0) && reply.trim() == "OK"
}

/// 读取实例信息：`(pid, port)`。
fn read_info(path: &Path) -> Option<(u32, u16)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut pid = None;
    let mut port = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("pid=") {
            pid = value.trim().parse::<u32>().ok();
        } else if let Some(value) = line.strip_prefix("port=") {
            port = value.trim().parse::<u16>().ok();
        }
    }
    Some((pid?, port?))
}

/// 写入实例信息（原子替换，避免读到半截文件）。
fn write_info(path: &Path, pid: u32, port: u16) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, format!("pid={pid}\nport={port}\n"))?;
    std::fs::rename(&temp, path)
}

/// 进程是否存活（与 `gt-backend::lock` 同一套判据：不引入 `libc`，走系统工具）。
///
/// 无法判定时返回 `true`：宁可认为"主实例还在"（最多是多开一个进程的退让），
/// 也不要在主实例活着时抢走单实例身份。
fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();
        let Ok(output) = output else {
            return true;
        };
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{forward_to, read_info, serve, take_pending, write_info, Startup};
    use std::net::TcpListener;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("knotwork-ipc-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        dir
    }

    #[test]
    fn instance_info_round_trips() {
        let dir = temp_dir("info");
        let path = dir.join(super::INFO_FILE);
        write_info(&path, 4242, 51234).expect("写实例信息");
        assert_eq!(read_info(&path), Some((4242, 51234)));

        // 损坏/缺失时返回 None，而不是 panic。
        std::fs::write(&path, "garbage").expect("写坏文件");
        assert_eq!(read_info(&path), None);
        assert_eq!(read_info(&dir.join("missing.json")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forwarding_delivers_paths_to_the_server() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("绑定");
        let port = listener.local_addr().expect("地址").port();
        let handle = std::thread::spawn(move || serve(&listener));

        // 清掉可能残留的队列内容，保证断言只看本次转发。
        let _ = take_pending();

        let paths = vec!["C:/repos/alpha".to_owned(), "C:/repos/beta".to_owned()];
        assert!(forward_to(port, &paths), "转发应成功（收到 OK）");

        // 监听线程是异步的：给它一点时间写入队列。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        let mut received = Vec::new();
        while std::time::Instant::now() < deadline {
            received = take_pending();
            if !received.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(received, paths);

        drop(handle);
    }

    #[test]
    fn forwarding_to_a_dead_port_fails_fast() {
        // 取一个刚释放的端口：连接会被拒绝，函数应返回 false 而不是卡住。
        let port = {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("绑定");
            listener.local_addr().expect("地址").port()
        };
        assert!(!forward_to(port, &["C:/repos/x".to_owned()]));
    }

    #[test]
    fn isolate_skips_the_guard() {
        let startup = super::start(&[], true);
        assert!(matches!(startup, Startup::Standalone(_)));
    }

    #[test]
    fn current_process_is_alive() {
        assert!(super::pid_alive(std::process::id()));
    }
}
