//! P3 独立后台开发入口。生产 GUI 切换完成前只允许显式隔离数据运行。
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
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!(
                    "QingMail P3 独立后台开发入口\n用法：mail-daemon --isolated-root <绝对目录> [--status|--quit]\n此阶段仅支持隔离运行，不读取正式账号、不联网、不发送邮件。正常应用仍使用 mail-ui。"
                );
                return Ok(());
            }
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
    let root = root.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "当前开发入口必须指定 --isolated-root；未打开正式数据",
        )
    })?;
    let config = Config::isolated(root)?;
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
