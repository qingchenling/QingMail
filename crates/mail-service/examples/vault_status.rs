use mail_service::credentials::{CredentialVault, SystemVault};
fn main() {
    match SystemVault.status() {
        Ok(()) => println!("系统密钥库可用（只读检查，未写入凭据）"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
