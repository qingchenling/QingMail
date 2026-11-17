//! P3 独立后台入口，支持正式运行与禁止邮件网络的隔离验证。
use mail_daemon::ipc::{Config, Server};
use mail_protocol::{Command, Reply, transport::Client};
use std::{io, path::PathBuf};
fn main() {
    if let Err(error) = run() {
        eprintln!("QingMail 后台：{error}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut root = None;
    let mut control = None;
    let mut production = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!(
                    "QingMail P3 独立后台\n用法：mail-daemon --isolated-root <绝对目录> [--status|--quit]\n正式运行：mail-daemon --production；通常由 mail-ui 自动启动。隔离运行不读取正式账号、不联网、不发送邮件。"
                );
                return Ok(());
            }
            Some("--production") if !production => production = true,
            Some("--isolated-root") if root.is_none() => root = args.next().map(PathBuf::from),
            Some("--status") if control.is_none() => control = Some(Command::Status),
            Some("--quit") if control.is_none() => control = Some(Command::PrepareShutdown),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "参数无效，请用 --help 查看用法",
                ));
            }
        }
    }
    let config = match (root, production) {
        (Some(root), false) => Config::isolated(root)?,
        (None, true) => Config::production()?,
        _ => {
            return Err(io::Error::other(
                "须选择 --production 或 --isolated-root，不能混用",
            ));
        }
    };
    if let Some(command) = control {
        match Client::new(config.runtime_root)
            .request(command)
            .map_err(io::Error::other)?
        {
            Reply::Status(status) => println!(
                "pid={} paused={} {}",
                status.pid, status.sync_paused, status.sync_status
            ),
            Reply::Acknowledged => println!("后台正在退出"),
            _ => return Err(io::Error::other("后台返回了非预期结果")),
        }
        return Ok(());
    }
    let server = Server::start(config)?;
    println!(
        "QingMail daemon ready · pid={} · {}",
        std::process::id(),
        server.socket_path().display()
    );
    server.run()
}
