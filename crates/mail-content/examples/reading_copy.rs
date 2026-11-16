//! cargo run -p mail-content --example reading_copy -- input.eml cache-parent
use std::{env, fs, path::Path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args_os().collect::<Vec<_>>();
    if arguments.len() != 3 {
        return Err("usage: reading_copy input.eml existing-private-cache-parent".into());
    }
    let content = mail_content::parse(&fs::read(&arguments[1])?)?;
    let store = mail_content::CopyStore::open(Path::new(&arguments[2]))?;
    let path = store.write(&content)?;
    println!("{}", path.display());
    eprintln!(
        "subject={} images={} remote_candidates={} warnings={:?}",
        content.subject,
        content.images.len(),
        content.remote_images.len(),
        content.warnings
    );
    Ok(())
}
